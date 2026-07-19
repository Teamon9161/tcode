//! Approval dialog with Tab-annotation: any option can carry a free-text
//! note. "Yes + note" lets the model adjust without redoing the work;
//! "No + note" tells it why. A change proposal's full diff is baked into
//! the transcript (scrollable there) while this dialog is open, so the
//! dialog itself carries only the choices; a decline retracts the diff.
//!
//! The same widget also serves `ask_user`: one or more questions rendered
//! as a paged form (←→ to switch questions, ↑↓ to choose, space to toggle
//! multi-select). All answers aggregate into a single note comment.

use std::cell::Cell;

use ratatui::text::{Line, Span};
use serde_json::Value;
use tcode_core::{Approval, ApprovalDecision, PermissionMode};

use crate::editor::Editor;
use crate::markdown::Document;
use crate::theme;

pub struct Dialog {
    pub summary: String,
    pub descriptor: String,
    /// Only ordinary tool authorization requests can add a project rule.
    project_option: bool,
    /// File mutation approvals offer a session-wide accept-edits transition
    /// instead of descriptor-specific allow rules.
    is_edit: bool,
    /// ToolStart-format call summary. A declined call never emits
    /// ToolStart, so the dialog supplies the line to bake instead.
    pub call_summary: String,
    selected: usize,
    /// Single-line note editor: full cursor movement, wraps on render.
    note: Editor,
    note_focused: bool,
    /// Present iff this is an `ask_user` question form (else a consent prompt).
    questions: Option<Questions>,
    /// Present iff this is a plan-review prompt: approving picks the mode
    /// execution runs under, declining returns feedback to keep planning. The
    /// plan body itself is baked into the transcript, not shown here.
    plan: Option<PlanReview>,
    /// The focused note caret's cell within the rendered dialog lines, set on
    /// each `render`. The frontend places the real terminal cursor there so the
    /// OS IME composition window has an anchor that tracks the caret. `None`
    /// when no note is focused.
    cursor_cell: Cell<Option<(u16, u16)>>,
    /// Rows occupied by the visible note editor in the most recent render.
    /// This lets mouse clicks use the same wrapped geometry as the caret.
    note_hitbox: Cell<Option<NoteHitbox>>,
    /// Current panel width, captured during render so ↑↓ follows visual wraps.
    note_width: Cell<u16>,
}

#[derive(Clone, Copy)]
struct NoteHitbox {
    first_row: usize,
    rows: usize,
}

/// The plan-review surface: the plan itself rendered inside the panel,
/// navigable block by block, plus the four decisions. A comment can anchor to
/// the focused block; a decline sends the comments (or free feedback) back so
/// the model keeps planning. The plan the model relies on lives in the ledger;
/// this pane is the human's review of it.
struct PlanReview {
    /// The verbatim `exit_plan` tool input (ledger truth). Kept so the pane can
    /// bake the plan through the same renderer replay uses, and so `$EDITOR`
    /// round-trips the exact plan source.
    input: Value,
    blocks: Vec<PlanBlock>,
    /// The block the keyboard is on (comment / navigation target).
    focus: usize,
    /// First visible wrapped plan row. Keyboard focus moves keep their block in
    /// view; direct scrolling deliberately keeps its own position so reviewers
    /// can inspect surrounding context without changing their comment target.
    scroll: Cell<usize>,
    /// The focus used for the last scroll reconciliation. A changed focus means
    /// keyboard navigation should bring that block into view; an unchanged focus
    /// leaves a mouse-chosen scroll position alone.
    rendered_focus: Cell<usize>,
    /// Selected decision option.
    cursor: usize,
    /// Whether ↑/↓ currently move through the decision options rather than plan
    /// blocks. Saving a comment lands here so its next action is explicit.
    options_focused: bool,
    /// Some while composing a comment on the focused block.
    compose: Option<PlanCommentDraft>,
    /// Mouse anchor and current head in visible plan-row coordinates while the
    /// user drags a passage to quote. Keeping both makes the selected passage
    /// visible before the comment composer opens.
    mouse_anchor: Option<PlanMousePosition>,
    mouse_head: Option<PlanMousePosition>,
    /// Free-form feedback for keep-planning when no per-block comments were left.
    feedback: Editor,
    feedback_focused: bool,
    /// A `$EDITOR` revision of the plan, when it differs from the original. On
    /// approval it becomes the actual `exit_plan` input and is mirrored to disk;
    /// on keep-planning it is sent back as a diff.
    revised: Option<String>,
}

/// A comment authored during plan review. `quote` is `None` for a keyboard
/// comment on the focused block and `Some` for a mouse-selected passage.
struct PlanComment {
    quote: Option<String>,
    text: String,
}

/// The active comment composer retains an optional selected-passage quote until
/// the user saves or cancels it.
struct PlanCommentDraft {
    quote: Option<String>,
    editor: Editor,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct PlanMousePosition {
    row: usize,
    col: usize,
}

/// One plan block (a heading, paragraph, code block, table, or list item) with
/// its parsed Markdown retained until the review pane knows its actual width.
struct PlanBlock {
    /// Verbatim markdown source, for quoting into feedback.
    source: String,
    document: Document,
    comments: Vec<PlanComment>,
}

impl PlanReview {
    /// The plan the model submitted (ledger truth).
    fn original(&self) -> &str {
        self.input["plan"].as_str().unwrap_or("")
    }

    /// The plan currently under review: the `$EDITOR` revision if any, else the
    /// original.
    fn source(&self) -> &str {
        self.revised.as_deref().unwrap_or_else(|| self.original())
    }

    /// A replacement input for an approved revised plan. The original tool use
    /// remains ledger history; the accompanying user note tells the model why
    /// the tool executed this final artifact instead.
    fn approved_input(&self) -> Option<Value> {
        self.revised.as_ref().map(|revised| {
            let mut input = self.input.clone();
            input["plan"] = Value::String(revised.clone());
            input
        })
    }

    fn has_revision(&self) -> bool {
        self.revised.is_some()
    }

    /// A unified diff from the original plan to the `$EDITOR` revision, for
    /// sending back as feedback. `None` when the plan was not revised.
    fn revision_diff(&self) -> Option<String> {
        let revised = self.revised.as_deref()?;
        let diff = similar::TextDiff::from_lines(self.original().trim(), revised.trim());
        Some(
            diff.unified_diff()
                .context_radius(3)
                .header("plan", "revised")
                .to_string(),
        )
    }
}

/// One plan-review option. Approving carries a permission-mode transition;
/// declining without feedback pauses the turn until the user says more.
struct PlanOption {
    label: &'static str,
    decision: ApprovalDecision,
    set_mode: Option<PermissionMode>,
}

const PLAN_OPTIONS: [PlanOption; 4] = [
    PlanOption {
        label: "Yes, and approve edits manually",
        decision: ApprovalDecision::Yes,
        set_mode: Some(PermissionMode::Default),
    },
    PlanOption {
        label: "Yes, and auto-accept edits",
        decision: ApprovalDecision::Yes,
        set_mode: Some(PermissionMode::AcceptEdits),
    },
    PlanOption {
        label: "Yes, and use auto mode",
        decision: ApprovalDecision::Yes,
        set_mode: Some(PermissionMode::Auto),
    },
    PlanOption {
        label: "No, keep planning",
        decision: ApprovalDecision::No,
        set_mode: None,
    },
];

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

const OPTIONS: [(&str, ApprovalDecision, Option<PermissionMode>); 2] = [
    ("Yes", ApprovalDecision::Yes, None),
    ("Yes, for this session", ApprovalDecision::YesSession, None),
];
const EDIT_OPTIONS: [(&str, ApprovalDecision, Option<PermissionMode>); 2] = [
    ("Yes", ApprovalDecision::Yes, None),
    (
        "Yes, allow all edits",
        ApprovalDecision::Yes,
        Some(PermissionMode::AcceptEdits),
    ),
];
const PROJECT_OPTION: (&str, ApprovalDecision, Option<PermissionMode>) = (
    "Yes, allow in this project",
    ApprovalDecision::YesProject,
    None,
);
const DENY_OPTION: (&str, ApprovalDecision, Option<PermissionMode>) =
    ("No", ApprovalDecision::No, None);

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

/// Blocks the plan pane jumps on PageUp/PageDown.
const PLAN_PAGE: usize = 5;
/// Rows the plan pane reserves for its comment/feedback editor when budgeting
/// the scrollable body (an editor row can wrap once).
const PLAN_EDITOR_ROWS: usize = 2;
/// The plan body never shrinks below this many rows, even on a short terminal.
const PLAN_MIN_VIEWPORT: usize = 3;

fn plan_viewport(height: u16, row_count: usize) -> usize {
    let overhead = 1 + PLAN_OPTIONS.len() + PLAN_EDITOR_ROWS + 1;
    (height as usize)
        .saturating_sub(overhead)
        .max(PLAN_MIN_VIEWPORT)
        .min(row_count)
}

pub enum DialogResult {
    Pending,
    Done(Approval),
    /// The plan pane asked to open the plan in `$EDITOR`. The frontend owns the
    /// terminal suspend/resume, so it handles this rather than the dialog.
    EditPlan,
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
    pub fn new(
        summary: String,
        descriptor: String,
        call_summary: String,
        is_edit: bool,
        project_option: bool,
    ) -> Self {
        Self {
            summary,
            descriptor,
            is_edit,
            project_option,
            call_summary,
            selected: 0,
            note: Editor::new(),
            note_focused: false,
            questions: None,
            plan: None,
            cursor_cell: Cell::new(None),
            note_hitbox: Cell::new(None),
            note_width: Cell::new(80),
        }
    }

    /// A plan-review prompt. The plan itself is the review surface: it renders
    /// inside this pane, navigable block by block. `title` names the plan;
    /// `input` is the verbatim `exit_plan` tool input; `blocks` pairs each
    /// block's verbatim source with its parsed Markdown.
    pub fn plan(title: String, input: Value, blocks: Vec<(String, Document)>) -> Self {
        let blocks: Vec<PlanBlock> = blocks
            .into_iter()
            .map(|(source, document)| PlanBlock {
                source,
                document,
                comments: Vec::new(),
            })
            .collect();
        Self {
            summary: title,
            descriptor: "exit_plan".into(),
            is_edit: false,
            project_option: false,
            call_summary: String::new(),
            selected: 0,
            note: Editor::new(),
            note_focused: false,
            questions: None,
            plan: Some(PlanReview {
                input,
                blocks,
                focus: 0,
                scroll: Cell::new(0),
                rendered_focus: Cell::new(0),
                cursor: 0,
                options_focused: false,
                compose: None,
                mouse_anchor: None,
                mouse_head: None,
                feedback: Editor::new(),
                feedback_focused: false,
                revised: None,
            }),
            cursor_cell: Cell::new(None),
            note_hitbox: Cell::new(None),
            note_width: Cell::new(80),
        }
    }

    /// The verbatim `exit_plan` input, for baking the plan into the transcript
    /// on decline through the same renderer replay uses.
    pub fn plan_input(&self) -> Option<Value> {
        self.plan.as_ref().map(|p| p.input.clone())
    }

    /// The current plan source (the `$EDITOR` revision if one was made, else the
    /// original), for writing to the review temp file.
    pub fn plan_source(&self) -> Option<String> {
        self.plan.as_ref().map(|p| p.source().to_string())
    }

    /// Adopt a `$EDITOR` revision: the pane now shows the revised plan (blocks
    /// pre-rendered by the frontend, which owns the markdown renderer), and
    /// earlier per-block comments are dropped because their anchors may be stale.
    /// Reopening the editor and restoring the original clears a prior revision.
    pub fn revise_plan(&mut self, revised: String, blocks: Vec<(String, Document)>) {
        let Some(plan) = self.plan.as_mut() else {
            return;
        };
        // An editor save that did not alter what is currently on screen is a
        // true no-op; it must not discard comments.
        if revised.trim() == plan.source().trim() {
            return;
        }
        let restores_original = revised.trim() == plan.original().trim();
        plan.blocks = blocks
            .into_iter()
            .map(|(source, document)| PlanBlock {
                source,
                document,
                comments: Vec::new(),
            })
            .collect();
        plan.focus = 0;
        plan.scroll.set(0);
        plan.rendered_focus.set(0);
        plan.revised = (!restores_original).then_some(revised);
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
            is_edit: false,
            project_option: false,
            call_summary: String::new(),
            selected: 0,
            note: Editor::new(),
            note_focused: false,
            questions: Some(Questions { pages, page: 0 }),
            plan: None,
            cursor_cell: Cell::new(None),
            note_hitbox: Cell::new(None),
            note_width: Cell::new(80),
        }
    }

    pub fn is_question(&self) -> bool {
        self.questions.is_some()
    }

    pub fn is_plan(&self) -> bool {
        self.plan.is_some()
    }

    /// Only plan review owns a scrollable body. Ordinary tool approvals keep
    /// their choices compact while the pre-baked change remains scrollable in
    /// the transcript behind the dialog.
    pub fn owns_wheel(&self) -> bool {
        self.is_plan()
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
        if let Some(plan) = self.plan.as_mut() {
            plan.feedback_focused = true;
            plan.feedback.insert_str(&text);
            return;
        }
        self.note_focused = true;
        if self.questions.is_some() {
            self.cur_page().note.insert_str(&text);
        } else {
            self.note.insert_str(&text);
        }
    }

    fn approval_options(&self) -> Vec<(&'static str, ApprovalDecision, Option<PermissionMode>)> {
        if self.is_edit {
            return EDIT_OPTIONS
                .into_iter()
                .chain(std::iter::once(DENY_OPTION))
                .collect();
        }
        let mut options = OPTIONS.to_vec();
        if self.project_option {
            options.push(PROJECT_OPTION);
        }
        options.push(DENY_OPTION);
        options
    }

    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> DialogResult {
        if self.plan.is_some() {
            return self.handle_plan_key(key);
        }
        if self.questions.is_some() {
            return self.handle_question_key(key);
        }
        use crossterm::event::KeyCode as K;
        let options = self.approval_options();
        let note_width = self.note_width.get();
        match key.code {
            K::Tab => self.note_focused = !self.note_focused,
            K::Enter => {
                let note = self.note_text();
                let (_, decision, set_mode) = options[self.selected];
                return DialogResult::Done(Approval {
                    decision,
                    comment: Some(note).filter(|s| !s.is_empty()),
                    set_mode,
                    approved_input: None,
                });
            }
            K::Esc => {
                if self.note_focused {
                    self.note_focused = false;
                } else {
                    return DialogResult::Done(Approval::simple(ApprovalDecision::No, None));
                }
            }
            K::Left if self.note_focused => self.note.left(),
            K::Right if self.note_focused => self.note.right(),
            K::Home if self.note_focused => self.note.home(),
            K::End if self.note_focused => self.note.end(),
            K::Delete if self.note_focused => self.note.delete(),
            K::Backspace if self.note_focused => self.note.backspace(),
            K::Up if self.note_focused => {
                move_wrapped_note_cursor(&mut self.note, false, note_width)
            }
            K::Down if self.note_focused => {
                move_wrapped_note_cursor(&mut self.note, true, note_width)
            }
            K::Up if !self.note_focused => {
                self.selected = self.selected.checked_sub(1).unwrap_or(options.len() - 1)
            }
            K::Down if !self.note_focused => {
                self.selected = (self.selected + 1) % options.len();
            }
            K::Char(c) if !self.note_focused && c.is_ascii_digit() => {
                let index = (c as usize).wrapping_sub('1' as usize);
                if index < options.len() {
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
        let note_width = self.note_width.get();
        match key.code {
            K::Tab => self.note_focused = !self.note_focused,
            K::Enter => return self.submit_or_advance(),
            K::Esc => {
                if self.note_focused {
                    self.note_focused = false;
                } else {
                    return DialogResult::Done(Approval::simple(ApprovalDecision::No, None));
                }
            }
            K::Left if focused => self.cur_page().note.left(),
            K::Right if focused => self.cur_page().note.right(),
            K::Home if focused => self.cur_page().note.home(),
            K::End if focused => self.cur_page().note.end(),
            K::Delete if focused => self.cur_page().note.delete(),
            K::Backspace if focused => self.cur_page().note.backspace(),
            K::Up if focused => {
                move_wrapped_note_cursor(&mut self.cur_page().note, false, note_width)
            }
            K::Down if focused => {
                move_wrapped_note_cursor(&mut self.cur_page().note, true, note_width)
            }
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
        DialogResult::Done(Approval::simple(ApprovalDecision::Yes, Some(comment)))
    }

    pub fn note_mouse_down(&mut self, row: usize, col: usize, width: u16) -> bool {
        let Some(hitbox) = self.note_hitbox.get() else {
            return false;
        };
        if row < hitbox.first_row || row >= hitbox.first_row + hitbox.rows {
            return false;
        }
        let row = row - hitbox.first_row;
        let display_col = col.saturating_sub(NOTE_INDENT);
        self.note_focused = true;
        if self.questions.is_some() {
            set_wrapped_note_cursor(&mut self.cur_page().note, row, display_col, width);
        } else {
            set_wrapped_note_cursor(&mut self.note, row, display_col, width);
        }
        true
    }

    fn note_rows(&self, note: &Editor, width: u16) -> WrappedEditor {
        let avail = (width as usize).saturating_sub(NOTE_INDENT + 2).max(10);
        wrap_editor(
            &note.text(),
            self.note_focused.then(|| note.cursor().1),
            avail,
        )
    }

    fn render_note(&self, note: &Editor, width: u16, out: &mut Vec<Line<'static>>) {
        let note_style = if self.note_focused {
            theme::accent()
        } else {
            theme::dim()
        };
        let first_row = out.len();
        let layout = self.note_rows(note, width);
        self.note_hitbox.set(Some(NoteHitbox {
            first_row,
            rows: layout.rows.len(),
        }));
        for (i, row) in layout.rows.iter().enumerate() {
            if let Some((_, caret_col)) = layout.caret.filter(|(row, _)| *row == i) {
                self.cursor_cell
                    .set(Some((out.len() as u16, (NOTE_INDENT + caret_col) as u16)));
            }
            let prefix = if i == 0 { "  note: " } else { "        " };
            out.push(Line::from(vec![
                Span::styled(prefix.to_string(), note_style),
                Span::raw(row.clone()),
            ]));
        }
    }

    /// The focused note caret's (row, col) within the lines from the last
    /// `render`, so the frontend can anchor the terminal cursor (and thus the
    /// IME) there. `None` when no note is focused.
    pub fn cursor_cell(&self) -> Option<(u16, u16)> {
        self.cursor_cell.get()
    }

    pub fn render(&self, width: u16, height: u16) -> Vec<Line<'static>> {
        // Recomputed below only when a note is focused; stale between renders.
        self.cursor_cell.set(None);
        self.note_hitbox.set(None);
        self.note_width.set(width);
        if self.plan.is_some() {
            return self.render_plan(width, height);
        }
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
        let options = self.approval_options();
        for (i, (label, decision, set_mode)) in options.iter().enumerate() {
            let marker = if i == self.selected { "▸ " } else { "  " };
            let label = match decision {
                ApprovalDecision::YesSession | ApprovalDecision::YesProject => {
                    format!("{label} ({})", approval_rule_label(&self.descriptor))
                }
                _ => (*label).to_string(),
            };
            let color = match decision {
                ApprovalDecision::Yes if set_mode.is_some() => theme::ACCENT,
                ApprovalDecision::Yes => theme::OK,
                ApprovalDecision::YesSession | ApprovalDecision::YesProject => theme::ACCENT,
                ApprovalDecision::No => theme::ERROR,
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
                options.len()
            ),
            theme::dim(),
        ));
        out
    }

    /// Convert a panel-relative mouse coordinate to a wrapped plan-body row.
    /// The title occupies content row zero; everything below the viewport is an
    /// option or editor rather than selectable plan text.
    fn plan_mouse_position(
        &self,
        row: usize,
        col: usize,
        width: u16,
        height: u16,
    ) -> Option<(PlanMousePosition, Vec<(usize, usize)>)> {
        let (rows, spans) = self.plan_rows(width);
        let viewport = plan_viewport(height, rows.len());
        let scroll = self.plan_scroll(&spans, viewport, rows.len());
        let body_row = row.checked_sub(1)?;
        let absolute_row = scroll + body_row;
        (body_row < viewport && absolute_row < rows.len()).then_some((
            PlanMousePosition {
                row: absolute_row,
                col,
            },
            spans,
        ))
    }

    /// Start a mouse selection inside the visible plan viewport. `row` and
    /// `col` are content coordinates inside the approval panel (after its
    /// border), supplied by the frontend that owns terminal geometry.
    pub fn plan_mouse_down(&mut self, row: usize, col: usize, width: u16, height: u16) {
        let Some(plan) = self.plan.as_ref() else {
            return;
        };
        if plan.compose.is_some() || plan.feedback_focused || row == 0 {
            return;
        }
        let Some((position, spans)) = self.plan_mouse_position(row, col, width, height) else {
            return;
        };
        let plan = self.plan.as_mut().expect("plan dialog");
        if let Some((block, _)) = spans
            .iter()
            .enumerate()
            .find(|(_, (start, len))| position.row >= *start && position.row < *start + *len)
        {
            plan.focus = block;
        }
        plan.mouse_anchor = Some(position);
        plan.mouse_head = Some(position);
    }

    /// Update the visible selection while a mouse drag stays inside the plan
    /// viewport. The final selection is still extracted on mouse-up.
    pub fn plan_mouse_drag(&mut self, row: usize, col: usize, width: u16, height: u16) {
        if !matches!(self.plan.as_ref(), Some(plan) if plan.mouse_anchor.is_some()) {
            return;
        }
        if let Some((position, _)) = self.plan_mouse_position(row, col, width, height) {
            self.plan.as_mut().expect("plan dialog").mouse_head = Some(position);
        }
    }

    /// Complete a mouse selection. A non-empty drag opens the comment composer
    /// with the selected display text quoted; a click simply moves block focus.
    pub fn plan_mouse_up(&mut self, row: usize, col: usize, width: u16, height: u16) {
        self.plan_mouse_drag(row, col, width, height);
        let Some((anchor, head)) = self
            .plan
            .as_mut()
            .and_then(|plan| Some((plan.mouse_anchor.take()?, plan.mouse_head.take()?)))
        else {
            return;
        };
        if head == anchor {
            return;
        }
        let (rows, spans) = self.plan_rows(width);
        let quote = selected_plan_text(&rows, anchor, head);
        if quote.is_empty() {
            return;
        }
        let plan = self.plan.as_mut().expect("plan dialog");
        if let Some((block, _)) = spans
            .iter()
            .enumerate()
            .find(|(_, (start, len))| anchor.row >= *start && anchor.row < *start + *len)
        {
            plan.focus = block;
        }
        plan.compose = Some(PlanCommentDraft {
            quote: Some(quote),
            editor: Editor::new(),
        });
        plan.options_focused = false;
    }

    /// Plan review. The plan renders inside the pane, block-navigable; a
    /// comment anchors to the focused block, a decision (approve → a mode, or
    /// keep-planning → feedback) resolves the pane. Three input sub-modes:
    /// composing a comment, editing free feedback, or browsing blocks/options.
    fn handle_plan_key(&mut self, key: crossterm::event::KeyEvent) -> DialogResult {
        use crossterm::event::KeyCode as K;
        let plan = self.plan.as_ref().expect("plan dialog");

        if plan.compose.is_some() {
            let plan = self.plan.as_mut().expect("plan dialog");
            let draft = plan.compose.as_mut().expect("compose");
            match key.code {
                K::Enter => {
                    let text = draft.editor.text().trim().to_string();
                    let quote = draft.quote.clone();
                    plan.compose = None;
                    if !text.is_empty() {
                        plan.blocks[plan.focus]
                            .comments
                            .push(PlanComment { quote, text });
                        // Saving an annotation is not consent. Park the focus on
                        // the decisions so the user can deliberately choose the
                        // next destination with arrows or Enter.
                        plan.options_focused = true;
                    }
                }
                K::Esc => plan.compose = None,
                K::Left => draft.editor.left(),
                K::Right => draft.editor.right(),
                K::Home => draft.editor.home(),
                K::End => draft.editor.end(),
                K::Delete => draft.editor.delete(),
                K::Backspace => draft.editor.backspace(),
                K::Char(c) => draft.editor.insert_char(c),
                _ => {}
            }
            return DialogResult::Pending;
        }

        if plan.feedback_focused {
            if matches!(key.code, K::Enter) {
                return self.submit_plan();
            }
            let plan = self.plan.as_mut().expect("plan dialog");
            match key.code {
                K::Tab | K::Esc => plan.feedback_focused = false,
                K::Left => plan.feedback.left(),
                K::Right => plan.feedback.right(),
                K::Home => plan.feedback.home(),
                K::End => plan.feedback.end(),
                K::Delete => plan.feedback.delete(),
                K::Backspace => plan.feedback.backspace(),
                K::Char(c) => plan.feedback.insert_char(c),
                _ => {}
            }
            return DialogResult::Pending;
        }

        let plan = self.plan.as_mut().expect("plan dialog");
        if plan.options_focused {
            match key.code {
                K::Enter => return self.submit_plan(),
                // Every plan decision may carry a user note. Approval notes
                // follow the normal append-only UserNote path; the
                // keep-planning choice sends the same text back as feedback.
                K::Tab => plan.feedback_focused = true,
                // Esc explicitly declines the review. With no note, the core
                // agent loop pauses rather than asking the model to guess.
                K::Esc => return self.decline_plan(),
                K::Right => plan.options_focused = false,
                K::Up | K::Char('k') => plan.cursor = plan.cursor.saturating_sub(1),
                K::Down | K::Char('j') => {
                    plan.cursor = (plan.cursor + 1).min(PLAN_OPTIONS.len() - 1)
                }
                K::Char(c) if c.is_ascii_digit() => {
                    let index = (c as usize).wrapping_sub('1' as usize);
                    if index < PLAN_OPTIONS.len() {
                        plan.cursor = index;
                    }
                }
                _ => {}
            }
            return DialogResult::Pending;
        }

        match key.code {
            K::Enter => return self.submit_plan(),
            K::Esc => return self.decline_plan(),
            K::Tab => plan.feedback_focused = true,
            K::Up | K::Char('k') => plan.focus = plan.focus.saturating_sub(1),
            K::Down | K::Char('j') => {
                plan.focus = (plan.focus + 1).min(plan.blocks.len().saturating_sub(1))
            }
            K::Home | K::Char('g') => plan.focus = 0,
            K::End | K::Char('G') => plan.focus = plan.blocks.len().saturating_sub(1),
            K::PageUp => plan.focus = plan.focus.saturating_sub(PLAN_PAGE),
            K::PageDown => {
                plan.focus = (plan.focus + PLAN_PAGE).min(plan.blocks.len().saturating_sub(1))
            }
            K::Left => plan.options_focused = true,
            K::Char('c') => {
                plan.compose = Some(PlanCommentDraft {
                    quote: None,
                    editor: Editor::new(),
                })
            }
            K::Char('e') => return DialogResult::EditPlan,
            K::Char(c) if c.is_ascii_digit() => {
                let index = (c as usize).wrapping_sub('1' as usize);
                if index < PLAN_OPTIONS.len() {
                    plan.cursor = index;
                    plan.options_focused = true;
                }
            }
            _ => {}
        }
        DialogResult::Pending
    }

    /// Keep planning: decline carrying the assembled comments (or free
    /// feedback) as the reason. Walking away with nothing is still a decline.
    fn decline_plan(&mut self) -> DialogResult {
        DialogResult::Done(Approval {
            decision: ApprovalDecision::No,
            comment: self.assemble_feedback(),
            set_mode: None,
            approved_input: None,
        })
    }

    fn submit_plan(&mut self) -> DialogResult {
        let (decision, set_mode, revised, approved_input) = {
            let plan = self.plan.as_ref().expect("plan dialog");
            let opt = &PLAN_OPTIONS[plan.cursor];
            (
                opt.decision,
                opt.set_mode,
                plan.revised.clone(),
                plan.approved_input(),
            )
        };
        if decision == ApprovalDecision::No {
            return self.decline_plan();
        }
        // Approval notes use the same append-only UserNote path as ordinary
        // approval annotations, so comments remain visible to the next turn and
        // after session replay.
        let mut parts = Vec::new();
        if let Some(revised) = revised {
            parts.push(format!(
                "The user edited the plan before approving. Use this revised plan as the source of truth for execution, not the earlier draft:\n\n{revised}"
            ));
        }
        if let Some(feedback) = self.comments_feedback() {
            parts.push(feedback);
        }
        DialogResult::Done(Approval {
            decision,
            comment: (!parts.is_empty()).then(|| parts.join("\n\n")),
            set_mode,
            approved_input,
        })
    }

    /// The feedback the model receives on keep-planning: each anchored comment
    /// as a `>`-quoted block plus the comment, then any free feedback. Anchoring
    /// to the quoted source is what makes a TUI review approach the desktop
    /// "comment on a passage" experience.
    fn assemble_feedback(&self) -> Option<String> {
        let plan = self.plan.as_ref().expect("plan dialog");
        let mut parts: Vec<String> = Vec::new();
        // A `$EDITOR` edit sent back for more planning travels as a diff so the
        // model sees exactly what changed.
        if let Some(diff) = plan.revision_diff() {
            parts.push(format!("The user edited the plan:\n\n{diff}"));
        }
        if let Some(comments) = self.comments_feedback() {
            parts.push(comments);
        }
        (!parts.is_empty()).then(|| parts.join("\n\n"))
    }

    fn comments_feedback(&self) -> Option<String> {
        let plan = self.plan.as_ref().expect("plan dialog");
        let mut parts: Vec<String> = Vec::new();
        for block in &plan.blocks {
            for comment in &block.comments {
                let quote = comment.quote.as_deref().unwrap_or(&block.source);
                parts.push(format!("{}\n\n{}", quote_source(quote), comment.text));
            }
        }
        let free = plan.feedback.text().trim().to_string();
        if !free.is_empty() {
            parts.push(free);
        }
        (!parts.is_empty()).then(|| parts.join("\n\n"))
    }

    /// The wrapped plan body as display rows plus, for each block, the
    /// `(start, len)` span of rows it occupies (comments and the trailing
    /// blank included) so the viewport can be scrolled to keep the focus in
    /// view.
    fn plan_rows(&self, width: u16) -> (Vec<Line<'static>>, Vec<(usize, usize)>) {
        let plan = self.plan.as_ref().expect("plan dialog");
        let text_w = (width as usize).saturating_sub(2).max(10);
        let mut rows: Vec<Line<'static>> = Vec::new();
        let mut spans: Vec<(usize, usize)> = Vec::new();
        for (i, block) in plan.blocks.iter().enumerate() {
            let start = rows.len();
            let focused = i == plan.focus;
            let gutter = if focused {
                Span::styled("▎ ", theme::accent())
            } else {
                Span::raw("  ")
            };
            let mut wrapped = crate::transcript::wrap_lines(
                block.document.lines_at(text_w.saturating_sub(1)),
                text_w,
            );
            if wrapped.is_empty() {
                wrapped.push(Line::default());
            }
            for (j, wl) in wrapped.into_iter().enumerate() {
                let mut line_spans = vec![gutter.clone()];
                line_spans.extend(wl.spans);
                // A commented block wears its markers on its first row.
                if j == 0 && !block.comments.is_empty() {
                    line_spans.push(Span::styled(
                        format!(" {}", superscripts(block.comments.len())),
                        theme::accent(),
                    ));
                }
                rows.push(Line::from(line_spans));
            }
            for (n, comment) in block.comments.iter().enumerate() {
                for (j, row) in wrap_cells(&comment.text, text_w.saturating_sub(4))
                    .into_iter()
                    .enumerate()
                {
                    let prefix = if j == 0 {
                        format!("  {} ", superscript(n + 1))
                    } else {
                        "    ".to_string()
                    };
                    rows.push(Line::from(vec![
                        Span::styled(prefix, theme::accent()),
                        Span::raw(row),
                    ]));
                }
            }
            rows.push(Line::default());
            spans.push((start, rows.len() - start));
        }
        if let Some((anchor, head)) = plan.mouse_anchor.zip(plan.mouse_head) {
            highlight_plan_selection(&mut rows, anchor, head);
        }
        (rows, spans)
    }

    /// Move the plan viewport without changing the focused block. This is kept
    /// available while composing feedback: review and annotation are independent
    /// activities, so the text field must not trap the reviewer at its cursor.
    pub fn plan_mouse_wheel(&self, up: bool, width: u16, height: u16) {
        let (rows, _) = self.plan_rows(width);
        let viewport = plan_viewport(height, rows.len());
        let plan = self.plan.as_ref().expect("plan dialog");
        let max_scroll = rows.len().saturating_sub(viewport);
        let next = if up {
            plan.scroll.get().saturating_sub(PLAN_PAGE)
        } else {
            (plan.scroll.get() + PLAN_PAGE).min(max_scroll)
        };
        plan.scroll.set(next);
        // The focus is already visible on screen. Mark it reconciled so the
        // next render preserves this deliberate scroll instead of snapping back.
        plan.rendered_focus.set(plan.focus);
    }

    /// Scroll offset that keeps a newly focused block visible. Mouse scrolling
    /// is intentionally independent: the review target need not be the same
    /// part of the plan the user is currently rereading.
    fn plan_scroll(&self, spans: &[(usize, usize)], k: usize, total: usize) -> usize {
        let plan = self.plan.as_ref().expect("plan dialog");
        let mut scroll = plan.scroll.get();
        if plan.rendered_focus.replace(plan.focus) != plan.focus {
            let (fs, fl) = spans.get(plan.focus).copied().unwrap_or((0, 0));
            if fs < scroll {
                scroll = fs;
            } else if fs + fl > scroll + k {
                scroll = (fs + fl).saturating_sub(k);
            }
        }
        scroll = scroll.min(total.saturating_sub(k));
        plan.scroll.set(scroll);
        scroll
    }

    fn render_plan(&self, width: u16, height: u16) -> Vec<Line<'static>> {
        let plan = self.plan.as_ref().expect("plan dialog");
        let mut out: Vec<Line<'static>> = Vec::new();

        // Title with the focus position.
        let mut title = vec![
            Span::styled("▤ Review plan: ", theme::accent()),
            Span::styled(self.summary.clone(), theme::bold()),
        ];
        if plan.blocks.len() > 1 {
            title.push(Span::styled(
                format!("  · block {}/{}", plan.focus + 1, plan.blocks.len()),
                theme::dim(),
            ));
        }
        out.push(Line::from(title));

        // Plan body, scrolled to keep the focus visible. Reserve the rest of
        // the panel for the options, the editor row, and the hint.
        let (rows, spans) = self.plan_rows(width);
        let k = plan_viewport(height, rows.len());
        let scroll = self.plan_scroll(&spans, k, rows.len());
        plan.scroll.set(scroll);
        out.extend(rows.into_iter().skip(scroll).take(k));

        // Decision options. After an external edit these are deliberately
        // phrased as the two available destinations: options 1–3 approve the
        // revised artifact under a chosen permission mode; option 4 sends its
        // diff back for another planning pass.
        for (i, opt) in PLAN_OPTIONS.iter().enumerate() {
            let label = if plan.has_revision() {
                match i {
                    0 => "Approve revised plan, approve edits manually",
                    1 => "Approve revised plan, auto-accept edits",
                    2 => "Approve revised plan, use auto mode",
                    _ => "Send revision back as feedback",
                }
            } else {
                opt.label
            };
            let selected = i == plan.cursor
                && plan.options_focused
                && !plan.feedback_focused
                && plan.compose.is_none();
            // The cursor's own row keeps its "·" affordance even while the
            // Tab-opened note editor has focus — otherwise every marker blanks
            // out at once and there's no way to tell which option a note
            // being typed applies to.
            let marker = if selected {
                "▸ "
            } else if i == plan.cursor && plan.compose.is_none() {
                "· "
            } else {
                "  "
            };
            let color = if opt.decision == ApprovalDecision::No {
                theme::ERROR
            } else {
                theme::OK
            };
            let style = if selected {
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

        // Composing a comment takes the editor row; otherwise it shows the
        // free-feedback line.
        if let Some(draft) = plan.compose.as_ref() {
            if let Some(quote) = &draft.quote {
                let quote = wrap_cells(quote, (width as usize).saturating_sub(16).max(10))
                    .into_iter()
                    .next()
                    .unwrap_or_default();
                out.push(Line::from(vec![
                    Span::styled("  quote: ", theme::dim()),
                    Span::styled(quote, theme::dim()),
                ]));
            }
            self.render_line_editor("comment:", &draft.editor, true, width, &mut out);
        } else {
            self.render_line_editor(
                "note:",
                &plan.feedback,
                plan.feedback_focused,
                width,
                &mut out,
            );
        }

        let hint = if plan.compose.is_some() {
            "  type comment · enter save · esc discard comment"
        } else if plan.feedback_focused {
            "  type note or feedback · enter confirm · esc return to plan"
        } else if plan.options_focused {
            "  ↑↓/1-4 choose · tab note · enter confirm · → return to plan · esc decline"
        } else if plan.has_revision() {
            "  ↑↓ blocks · c comment · e edit · ←/1-4 choose · esc = keep planning"
        } else {
            "  ↑↓ blocks · c comment · drag text to quote · ←/1-4 choose · tab feedback · esc = keep planning"
        };
        out.push(Line::styled(hint, theme::dim()));
        out
    }

    /// A single-line editor row (comment or feedback), styled like the consent
    /// note with a labelled prefix and a cursor bar when focused.
    fn render_line_editor(
        &self,
        label: &str,
        editor: &Editor,
        focused: bool,
        width: u16,
        out: &mut Vec<Line<'static>>,
    ) {
        let avail = (width as usize).saturating_sub(NOTE_INDENT + 2).max(10);
        let layout = wrap_editor(&editor.text(), focused.then(|| editor.cursor().1), avail);
        let style = if focused {
            theme::accent()
        } else {
            theme::dim()
        };
        let indent = " ".repeat(NOTE_INDENT);
        for (i, row) in layout.rows.iter().enumerate() {
            let prefix = if i == 0 {
                format!("  {label} ")
            } else {
                indent.clone()
            };
            if let Some((_, caret_col)) = layout.caret.filter(|(row, _)| *row == i) {
                use unicode_width::UnicodeWidthStr;
                self.cursor_cell.set(Some((
                    out.len() as u16,
                    (UnicodeWidthStr::width(prefix.as_str()) + caret_col) as u16,
                )));
            }
            out.push(Line::from(vec![
                Span::styled(prefix, style),
                Span::raw(row.clone()),
            ]));
        }
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

/// Prefix every line of a block's source with `> ` so a comment quotes the
/// exact passage it refers to.
fn quote_source(source: &str) -> String {
    source
        .lines()
        .map(|line| format!("> {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A single superscript digit for a comment index (1-based); falls back to a
/// bracketed number past nine.
fn superscript(n: usize) -> String {
    const SUP: [char; 10] = ['⁰', '¹', '²', '³', '⁴', '⁵', '⁶', '⁷', '⁸', '⁹'];
    if (1..=9).contains(&n) {
        SUP[n].to_string()
    } else {
        format!("[{n}]")
    }
}

/// The run of superscript markers a block wears when it carries `count`
/// comments (¹²³ …).
fn superscripts(count: usize) -> String {
    (1..=count).map(superscript).collect()
}

/// Apply the normal terminal selection treatment to the part of each wrapped
/// row covered by a plan drag. The original Markdown styles survive; only the
/// selected cells gain the reversed modifier.
fn highlight_plan_selection(
    rows: &mut [Line<'static>],
    a: PlanMousePosition,
    b: PlanMousePosition,
) {
    use ratatui::style::Modifier;
    use unicode_width::UnicodeWidthChar;

    let (start, end) = if (a.row, a.col) <= (b.row, b.col) {
        (a, b)
    } else {
        (b, a)
    };
    for (row_index, line) in rows
        .iter_mut()
        .enumerate()
        .skip(start.row)
        .take(end.row.saturating_sub(start.row) + 1)
    {
        let from = if row_index == start.row {
            start.col.max(2)
        } else {
            2
        };
        let to = if row_index == end.row {
            end.col.max(2)
        } else {
            usize::MAX
        };
        let mut cell = 0usize;
        let mut spans = Vec::new();
        for span in std::mem::take(&mut line.spans) {
            for c in span.content.chars() {
                let width = c.width().unwrap_or(0);
                let selected = cell + width > from && cell < to;
                let style = if selected {
                    span.style.add_modifier(Modifier::REVERSED)
                } else {
                    span.style
                };
                spans.push(Span::styled(c.to_string(), style));
                cell += width;
            }
        }
        line.spans = spans;
    }
}

/// Extract the text under a plan-pane mouse drag. Plan rows carry a two-cell
/// gutter; selection columns are panel-relative, so remove that furniture before
/// slicing and never include comments/options outside the visible plan rows.
fn selected_plan_text(rows: &[Line<'_>], a: PlanMousePosition, b: PlanMousePosition) -> String {
    use unicode_width::UnicodeWidthChar;

    let (start, end) = if (a.row, a.col) <= (b.row, b.col) {
        (a, b)
    } else {
        (b, a)
    };
    let mut selected = Vec::new();
    for row in start.row..=end.row {
        let text = rows
            .get(row)
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .unwrap_or_default();
        let from = if row == start.row {
            start.col.max(2)
        } else {
            2
        };
        let to = if row == end.row {
            end.col.max(2)
        } else {
            usize::MAX
        };
        let mut cell = 0usize;
        let mut part = String::new();
        for c in text.chars() {
            let width = c.width().unwrap_or(0);
            if cell + width > from && cell < to {
                part.push(c);
            }
            cell += width;
            if cell >= to {
                break;
            }
        }
        let part = part.trim();
        if !part.is_empty() {
            selected.push(part.to_string());
        }
    }
    selected.join("\n")
}

fn move_wrapped_note_cursor(editor: &mut Editor, down: bool, width: u16) {
    use unicode_width::UnicodeWidthStr;

    let avail = (width as usize).saturating_sub(NOTE_INDENT + 2).max(10);
    let rows = wrap_cells(&editor.text(), avail);
    let display_col = editor.cursor().1;
    let mut base = 0usize;
    let current = rows
        .iter()
        .position(|line| {
            let end = base + line.width();
            let found = display_col <= end;
            if !found {
                base = end;
            }
            found
        })
        .unwrap_or_else(|| rows.len().saturating_sub(1));
    let target = if down {
        (current + 1).min(rows.len().saturating_sub(1))
    } else {
        current.saturating_sub(1)
    };
    if target == current {
        return;
    }
    let target_base: usize = rows.iter().take(target).map(|line| line.width()).sum();
    editor.set_cursor_by_display_col(
        0,
        target_base + (display_col - base).min(rows[target].width()),
    );
}

fn set_wrapped_note_cursor(editor: &mut Editor, row: usize, col: usize, width: u16) {
    use unicode_width::UnicodeWidthStr;

    let avail = (width as usize).saturating_sub(NOTE_INDENT + 2).max(10);
    let rows = wrap_cells(&editor.text(), avail);
    let base: usize = rows
        .iter()
        .take(row.min(rows.len().saturating_sub(1)))
        .map(|line| line.width())
        .sum();
    editor.set_cursor_by_display_col(0, base + col);
}

/// A persisted-rule option label lives on its own single `Line`, unlike the
/// summary above it (which wraps). A long or multi-line command — the
/// descriptor doubles as the raw rule pattern now that it's shown verbatim —
/// must not corrupt that row, so collapse to the first line and cap width.
fn approval_rule_label(descriptor: &str) -> String {
    const MAX_CHARS: usize = 60;
    let first_line = descriptor.lines().next().unwrap_or("");
    let overflows = first_line.chars().count() > MAX_CHARS || descriptor.lines().count() > 1;
    let mut label: String = first_line.chars().take(MAX_CHARS).collect();
    if overflows {
        label.push('…');
    }
    label
}

/// Rows of an editor and the logical terminal cell for its caret. The caret is
/// deliberately metadata, not a rendered glyph: inserting a sentinel into the
/// text would shift the suffix and change where a line wraps.
struct WrappedEditor {
    rows: Vec<String>,
    caret: Option<(usize, usize)>,
}

fn wrap_editor(text: &str, caret: Option<usize>, width: usize) -> WrappedEditor {
    use unicode_width::UnicodeWidthStr;

    let rows = wrap_cells(text, width);
    let mut starts = Vec::with_capacity(rows.len());
    let mut start = 0usize;
    for row in &rows {
        starts.push(start);
        start += UnicodeWidthStr::width(row.as_str());
    }

    // At a soft-wrap boundary, prefer the following row. This matches the
    // prompt composer and leaves the cursor before the character it addresses.
    let caret = caret.and_then(|caret| {
        rows.iter()
            .enumerate()
            .rposition(|(index, row)| {
                let start = starts[index];
                caret >= start && caret <= start + UnicodeWidthStr::width(row.as_str())
            })
            .map(|index| (index, caret.saturating_sub(starts[index])))
    });

    WrappedEditor { rows, caret }
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
            "run cargo test".into(),
            "run(cargo test)".into(),
            "run(cargo test)".into(),
            false,
            false,
        )
    }

    fn edit_dialog() -> Dialog {
        Dialog::new(
            "edit src/main.rs".into(),
            "edit(src/main.rs)".into(),
            "edit(src/main.rs)".into(),
            true,
            true,
        )
    }

    #[test]
    fn approval_rule_label_collapses_multiline_and_long_commands() {
        assert_eq!(approval_rule_label("run(cargo build)"), "run(cargo build)");
        let multiline = "run(cargo build\n&& cargo test)";
        assert_eq!(approval_rule_label(multiline), "run(cargo build…");
        let long = format!("run({})", "x".repeat(100));
        let label = approval_rule_label(&long);
        assert_eq!(label.chars().count(), 61); // 60 chars + ellipsis
        assert!(label.ends_with('…'));
    }

    #[test]
    fn only_plan_review_owns_the_mouse_wheel() {
        assert!(!dialog().owns_wheel());
        let plan = Dialog::plan(
            "Review plan".into(),
            json!({ "plan": "# Plan" }),
            vec![(
                "# Plan".into(),
                crate::markdown::Renderer::default().parse("# Plan"),
            )],
        );
        assert!(plan.owns_wheel());
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
        d.render(width, 40)
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
    fn note_caret_cell_tracks_the_focused_cursor() {
        let mut d = dialog();
        // No focused note yet: nothing for the terminal cursor to anchor to.
        let _ = d.render(80, 40);
        assert_eq!(d.cursor_cell(), None);

        d.handle_key(key(KeyCode::Tab));
        type_str(&mut d, "abc");
        let _ = d.render(80, 40);
        // Caret sits after "abc": col = NOTE_INDENT (8) + 3.
        let (_, col) = d.cursor_cell().expect("focused note exposes a caret cell");
        assert_eq!(col, (NOTE_INDENT + 3) as u16);

        // Moving the caret left shifts the reported cell left too.
        d.handle_key(key(KeyCode::Left));
        let _ = d.render(80, 40);
        let (_, col) = d.cursor_cell().expect("caret cell after Left");
        assert_eq!(col, (NOTE_INDENT + 2) as u16);
    }

    #[test]
    fn note_caret_uses_logical_columns_for_wide_characters() {
        let mut d = dialog();
        d.handle_key(key(KeyCode::Tab));
        type_str(&mut d, "a中b");
        d.handle_key(key(KeyCode::Left));
        let _ = d.render(80, 40);
        let (_, col) = d.cursor_cell().expect("caret after wide character");
        assert_eq!(col, (NOTE_INDENT + 3) as u16);
    }

    #[test]
    fn note_cursor_does_not_insert_a_rendered_cell() {
        let mut d = dialog();
        d.handle_key(key(KeyCode::Tab));
        type_str(&mut d, "abc");
        d.handle_key(key(KeyCode::Left));

        let screen = screen(&d, 80);
        assert!(
            screen.contains("note: abc"),
            "note text stays contiguous: {screen}"
        );
        assert!(
            !screen.contains('▏'),
            "the terminal cursor, not a rendered character, marks the caret: {screen}"
        );
    }

    #[test]
    fn wrapped_editor_preserves_text_and_places_boundary_caret_on_next_row() {
        let layout = wrap_editor("abcdefghi", Some(6), 6);
        assert_eq!(layout.rows, ["abcdef", "ghi"]);
        assert_eq!(layout.caret, Some((1, 0)));

        let layout = wrap_editor("a中b", Some(3), 10);
        assert_eq!(layout.rows, ["a中b"]);
        assert_eq!(layout.caret, Some((0, 3)));
    }

    #[test]
    fn clicking_a_note_moves_its_editor_cursor() {
        let mut d = dialog();
        d.handle_key(key(KeyCode::Tab));
        type_str(&mut d, "a中b");
        let _ = d.render(80, 40);
        let (row, _) = d.cursor_cell().expect("note cursor cell");
        assert!(d.note_mouse_down(row as usize, NOTE_INDENT + 1, 80));
        type_str(&mut d, "X");
        let DialogResult::Done(approval) = d.handle_key(key(KeyCode::Enter)) else {
            panic!("enter should submit the annotation");
        };
        assert_eq!(approval.comment.as_deref(), Some("aX中b"));
    }

    #[test]
    fn note_up_and_down_follow_visual_wrapping() {
        let mut d = dialog();
        d.handle_key(key(KeyCode::Tab));
        type_str(&mut d, &"x".repeat(25));
        let _ = d.render(30, 40);
        d.handle_key(key(KeyCode::Up));
        type_str(&mut d, "A");
        d.handle_key(key(KeyCode::Home));
        d.handle_key(key(KeyCode::Down));
        type_str(&mut d, "B");
        let DialogResult::Done(approval) = d.handle_key(key(KeyCode::Enter)) else {
            panic!("enter should submit the annotation");
        };
        let expected = format!("{}A{}B{}", "x".repeat(5), "x".repeat(14), "x".repeat(6));
        assert_eq!(approval.comment.as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn long_note_wraps_and_grows_height() {
        let mut d = dialog();
        d.handle_key(key(KeyCode::Tab));
        type_str(&mut d, &"x".repeat(60));
        // 40 cells wide leaves ~30 for the note: expect several rows.
        let rows = d
            .render(40, 40)
            .iter()
            .filter(|l| {
                let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
                text.contains('x')
            })
            .count();
        assert!(rows >= 2, "60-char note must wrap at width 40");
        assert!(d.render(40, 40).len() > d.render(200, 40).len());
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
    fn edit_approval_switches_to_accept_edits_without_a_descriptor_rule() {
        let mut d = edit_dialog();
        let screen = screen(&d, 80);
        assert!(screen.contains("Yes, allow all edits"));
        assert!(!screen.contains("for this session"));
        assert!(!screen.contains("allow in this project"));

        d.handle_key(key(KeyCode::Down));
        let DialogResult::Done(approval) = d.handle_key(key(KeyCode::Enter)) else {
            panic!("enter should confirm the edit-mode transition");
        };
        assert_eq!(approval.decision, ApprovalDecision::Yes);
        assert_eq!(approval.set_mode, Some(PermissionMode::AcceptEdits));
    }

    #[test]
    fn arrows_and_always() {
        let mut d = dialog();
        d.handle_key(key(KeyCode::Down));
        let DialogResult::Done(a) = d.handle_key(key(KeyCode::Enter)) else {
            panic!("enter should confirm");
        };
        assert_eq!(a.decision, ApprovalDecision::YesSession);
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

    /// Parsed blocks retain table structure until the dialog has its final width.
    fn blocks_for(src: &str) -> Vec<(String, Document)> {
        let markdown = crate::markdown::Renderer::default();
        crate::markdown::split_blocks(src)
            .into_iter()
            .map(|block| {
                let document = markdown.parse(&block);
                (block, document)
            })
            .collect()
    }

    /// Build a plan dialog whose blocks each render to one line of their source.
    fn plan_dialog(src: &str) -> Dialog {
        Dialog::plan(
            "Test plan".into(),
            json!({ "plan": src, "title": "Test plan" }),
            blocks_for(src),
        )
    }

    fn render_text(d: &Dialog, width: u16, height: u16) -> String {
        d.render(width, height)
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

    #[test]
    fn plan_esc_with_nothing_left_just_keeps_planning() {
        let mut d = plan_dialog("# Title\n\nBody paragraph.");
        assert!(d.is_plan());
        let DialogResult::Done(a) = d.handle_key(key(KeyCode::Esc)) else {
            panic!("esc keeps planning");
        };
        assert_eq!(a.decision, ApprovalDecision::No);
        assert_eq!(a.comment, None);
        assert_eq!(a.set_mode, None);
    }

    #[test]
    fn plan_default_approval_requires_manual_edit_approval() {
        let mut d = plan_dialog("# T\n\nBody.");
        let DialogResult::Done(a) = d.handle_key(key(KeyCode::Enter)) else {
            panic!("enter approves");
        };
        assert_eq!(a.decision, ApprovalDecision::Yes);
        assert_eq!(a.set_mode, Some(PermissionMode::Default));
        assert_eq!(a.comment, None);
    }

    #[test]
    fn plan_left_enters_approval_and_right_returns_to_the_body() {
        let mut d = plan_dialog("Body.");
        d.handle_key(key(KeyCode::Left));
        assert!(
            d.plan.as_ref().expect("plan dialog").options_focused,
            "left exits the plan body into approval choices"
        );
        d.handle_key(key(KeyCode::Right));
        assert!(
            !d.plan.as_ref().expect("plan dialog").options_focused,
            "right returns from approval choices to the plan body"
        );
    }

    #[test]
    fn plan_digit_picks_the_auto_mode_option() {
        let mut d = plan_dialog("# T\n\nBody.");
        d.handle_key(key(KeyCode::Char('3'))); // "use auto mode"
        let DialogResult::Done(a) = d.handle_key(key(KeyCode::Enter)) else {
            panic!("enter approves");
        };
        assert_eq!(a.set_mode, Some(PermissionMode::Auto));
    }

    #[test]
    fn plan_comment_anchors_to_the_focused_block() {
        let mut d = plan_dialog("First block.\n\nSecond block.");
        d.handle_key(key(KeyCode::Down)); // focus the second block
        d.handle_key(key(KeyCode::Char('c')));
        type_str(&mut d, "reword this");
        d.handle_key(key(KeyCode::Enter)); // save the comment, focus decisions
        let DialogResult::Done(a) = d.handle_key(key(KeyCode::Esc)) else {
            panic!("Esc keeps planning with the comment");
        };
        assert_eq!(a.decision, ApprovalDecision::No);
        let comment = a.comment.expect("assembled feedback");
        assert!(
            comment.contains("> Second block."),
            "quotes the anchored block: {comment}"
        );
        assert!(comment.contains("reword this"));
        assert!(
            !comment.contains("First block"),
            "an uncommented block is not quoted: {comment}"
        );
    }

    #[test]
    fn saved_comment_moves_to_options_and_can_select_auto_accept() {
        let mut d = plan_dialog("Body.");
        d.handle_key(key(KeyCode::Char('c')));
        type_str(&mut d, "looks good");
        d.handle_key(key(KeyCode::Enter));
        d.handle_key(key(KeyCode::Down)); // manual approval → auto-accept
        let DialogResult::Done(a) = d.handle_key(key(KeyCode::Enter)) else {
            panic!("enter confirms the selected option");
        };
        assert_eq!(a.set_mode, Some(PermissionMode::AcceptEdits));
    }

    #[test]
    fn mouse_selected_passage_is_highlighted_and_comment_is_not_duplicated() {
        use ratatui::style::Modifier;

        let mut d = plan_dialog("First block.\n\nSecond block.");
        // Title is content row 0; the second block is visual plan-body row 2.
        d.plan_mouse_down(3, 2, 80, 20);
        d.plan_mouse_drag(3, 8, 80, 20);
        assert!(
            d.render(80, 20)
                .iter()
                .flat_map(|line| &line.spans)
                .any(|span| span.style.add_modifier.contains(Modifier::REVERSED)),
            "a drag visibly marks the selected passage"
        );
        d.plan_mouse_up(3, 8, 80, 20);
        type_str(&mut d, "make this concrete");
        d.handle_key(key(KeyCode::Enter));
        let rendered = render_text(&d, 80, 20);
        assert!(
            rendered.contains("Second block. ¹"),
            "block wears the comment marker: {rendered}"
        );
        assert!(
            rendered.contains("make this concrete"),
            "comment is visible: {rendered}"
        );
        assert!(
            !rendered.contains("› Second"),
            "the selected text is not repeated: {rendered}"
        );
        assert_eq!(
            rendered.matches("Second block.").count(),
            1,
            "only the plan body shows the source"
        );
    }

    #[test]
    fn approving_a_plan_preserves_its_comments_as_a_user_note() {
        let mut d = plan_dialog("First block.\n\nSecond block.");
        d.plan_mouse_down(3, 2, 80, 20);
        d.plan_mouse_up(3, 8, 80, 20);
        type_str(&mut d, "make this concrete");
        d.handle_key(key(KeyCode::Enter));
        let DialogResult::Done(a) = d.handle_key(key(KeyCode::Enter)) else {
            panic!("approval should complete");
        };
        assert_eq!(a.decision, ApprovalDecision::Yes);
        let feedback = a.comment.expect("approved comment becomes a user note");
        assert!(
            feedback.contains("> Second"),
            "exact selected passage: {feedback}"
        );
        assert!(feedback.contains("make this concrete"));
        assert!(
            !feedback.contains("First block"),
            "only the selection is quoted: {feedback}"
        );
    }

    #[test]
    fn escape_from_plan_options_declines_without_feedback() {
        let mut d = plan_dialog("Body.");
        d.handle_key(key(KeyCode::Char('4')));
        let DialogResult::Done(a) = d.handle_key(key(KeyCode::Esc)) else {
            panic!("Esc from plan options must decline immediately");
        };
        assert_eq!(a.decision, ApprovalDecision::No);
        assert_eq!(a.comment, None);
    }

    #[test]
    fn plan_keep_planning_without_feedback_pauses_the_turn() {
        let mut d = plan_dialog("Body.");
        d.handle_key(key(KeyCode::Char('4')));
        let DialogResult::Done(a) = d.handle_key(key(KeyCode::Enter)) else {
            panic!("enter must decline without forcing a feedback editor");
        };
        assert_eq!(a.decision, ApprovalDecision::No);
        assert_eq!(a.comment, None);
    }

    #[test]
    fn plan_keep_planning_tab_opens_its_feedback_editor() {
        let mut d = plan_dialog("Body.");
        d.handle_key(key(KeyCode::Char('4')));
        d.handle_key(key(KeyCode::Tab));
        assert!(
            d.plan.as_ref().expect("plan dialog").feedback_focused,
            "Tab on keep-planning must edit its feedback, not return to the plan"
        );
        type_str(&mut d, "cover the error path");
        let DialogResult::Done(a) = d.handle_key(key(KeyCode::Enter)) else {
            panic!("enter sends the keep-planning feedback");
        };
        assert_eq!(a.decision, ApprovalDecision::No);
        assert_eq!(a.comment.as_deref(), Some("cover the error path"));
    }

    #[test]
    fn plan_tab_opens_a_note_editor_for_approval_options() {
        let mut d = plan_dialog("Body.");
        d.handle_key(key(KeyCode::Char('2'))); // auto-accept edits
        d.handle_key(key(KeyCode::Tab));
        assert!(
            d.plan.as_ref().expect("plan dialog").feedback_focused,
            "Tab on an approval choice must open its note editor"
        );
        type_str(&mut d, "run the focused tests afterwards");
        let DialogResult::Done(a) = d.handle_key(key(KeyCode::Enter)) else {
            panic!("enter approves the plan with the note");
        };
        assert_eq!(a.decision, ApprovalDecision::Yes);
        assert_eq!(a.set_mode, Some(PermissionMode::AcceptEdits));
        assert_eq!(
            a.comment.as_deref(),
            Some("run the focused tests afterwards")
        );
    }

    #[test]
    fn plan_tab_note_keeps_the_chosen_option_marked() {
        let mut d = plan_dialog("Body.");
        d.handle_key(key(KeyCode::Char('2'))); // auto-accept edits
                                               // Regression: opening the Tab note editor used to blank every
                                               // option's marker, leaving no way to tell which one a note applies to.
        let before = render_text(&d, 60, 20);
        assert!(
            before.contains("▸ 2."),
            "option 2 should be fully selected before Tab"
        );

        d.handle_key(key(KeyCode::Tab));
        let after = render_text(&d, 60, 20);
        assert!(
            after.contains("· 2."),
            "option 2 must keep a visible marker while the note editor has focus:\n{after}"
        );
    }

    #[test]
    fn plan_wheel_scrolls_without_moving_the_comment_target() {
        let src = (1..=20)
            .map(|i| format!("Block number {i}."))
            .collect::<Vec<_>>()
            .join("\n\n");
        let mut d = plan_dialog(&src);
        let _ = render_text(&d, 60, 14); // establish the initial viewport
        d.handle_key(key(KeyCode::Char('4')));
        d.handle_key(key(KeyCode::Tab));
        assert!(d.plan.as_ref().expect("plan dialog").feedback_focused);

        d.plan_mouse_wheel(false, 60, 14);
        let text = render_text(&d, 60, 14);
        assert!(
            text.contains("Block number 4."),
            "wheel advances the body: {text}"
        );
        assert!(
            !text.contains("Block number 1."),
            "the old top row has scrolled away: {text}"
        );
        assert_eq!(
            d.plan.as_ref().expect("plan dialog").focus,
            0,
            "scrolling does not move the comment target"
        );
    }

    #[test]
    fn plan_editor_revision_is_approved_as_the_new_source_of_truth() {
        let mut d = plan_dialog("Original body.");
        d.revise_plan("Revised body.".into(), blocks_for("Revised body."));
        let DialogResult::Done(a) = d.handle_key(key(KeyCode::Enter)) else {
            panic!("enter approves");
        };
        assert_eq!(a.decision, ApprovalDecision::Yes);
        assert_eq!(a.set_mode, Some(PermissionMode::Default));
        let comment = a.comment.expect("the revision travels with the approval");
        assert!(comment.contains("Revised body."), "{comment}");
        assert!(comment.contains("revised plan"), "{comment}");
        assert_eq!(
            a.approved_input
                .as_ref()
                .and_then(|input| input["plan"].as_str()),
            Some("Revised body."),
            "the mirror receives the reviewed plan, not the original tool input"
        );
    }

    #[test]
    fn plan_editor_revision_can_be_sent_back_as_a_diff() {
        let mut d = plan_dialog("Original body.");
        d.revise_plan("Revised body.".into(), blocks_for("Revised body."));
        d.handle_key(key(KeyCode::Char('4'))); // keep planning
        let DialogResult::Done(a) = d.handle_key(key(KeyCode::Enter)) else {
            panic!("enter submits the edit as feedback");
        };
        assert_eq!(a.decision, ApprovalDecision::No);
        let comment = a.comment.expect("the edit is the feedback");
        assert!(comment.contains("The user edited the plan"), "{comment}");
        assert!(
            comment.contains("-Original body."),
            "a diff line: {comment}"
        );
        assert!(comment.contains("+Revised body."), "a diff line: {comment}");
    }

    #[test]
    fn plan_editor_revision_surfaces_approve_or_feedback_destinations() {
        let mut d = plan_dialog("Original body.");
        d.revise_plan("Revised body.".into(), blocks_for("Revised body."));
        let text = render_text(&d, 80, 20);
        assert!(
            text.contains("Approve revised plan, auto-accept edits"),
            "{text}"
        );
        assert!(text.contains("Send revision back as feedback"), "{text}");
    }

    #[test]
    fn restoring_the_original_plan_clears_a_prior_revision() {
        let mut d = plan_dialog("Original body.");
        d.revise_plan("Revised body.".into(), blocks_for("Revised body."));
        d.revise_plan("Original body.".into(), blocks_for("Original body."));
        let DialogResult::Done(a) = d.handle_key(key(KeyCode::Enter)) else {
            panic!("enter approves");
        };
        assert_eq!(a.comment, None);
        assert_eq!(a.approved_input, None);
    }

    #[test]
    fn plan_editor_no_change_adds_nothing() {
        let mut d = plan_dialog("Same body.");
        d.revise_plan("Same body.".into(), blocks_for("Same body."));
        let DialogResult::Done(a) = d.handle_key(key(KeyCode::Enter)) else {
            panic!("enter approves");
        };
        assert_eq!(a.comment, None, "an unchanged edit is a no-op");
    }

    #[test]
    fn plan_e_key_requests_an_external_edit() {
        let mut d = plan_dialog("Body.");
        assert!(matches!(
            d.handle_key(key(KeyCode::Char('e'))),
            DialogResult::EditPlan
        ));
    }

    #[test]
    fn plan_scrolls_to_keep_the_focused_block_visible() {
        let src = (1..=20)
            .map(|i| format!("Block number {i}."))
            .collect::<Vec<_>>()
            .join("\n\n");
        let mut d = plan_dialog(&src);
        d.handle_key(key(KeyCode::End)); // jump to the last block
        let text = render_text(&d, 60, 14);
        assert!(
            text.contains("Block number 20"),
            "the focus is in view: {text}"
        );
        assert!(
            !text.contains("Block number 1."),
            "an out-of-view block is not rendered: {text}"
        );
    }
}
