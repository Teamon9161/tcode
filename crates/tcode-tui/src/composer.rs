//! The prompt composer: how the input box is laid out, and how `@` file
//! references are matched, scored and highlighted.
//!
//! Everything here is a pure function of the editor state and the terminal
//! width. `App` keeps the state and decides *when* to re-layout; this module
//! decides only what the result looks like, which is what makes the layout
//! testable without a terminal.

use ratatui::text::Span;
use tcode_core::{ReferenceCandidate, ReferenceKind};
use unicode_width::UnicodeWidthChar;

use crate::editor::{Editor, Position};
use crate::theme;

pub const PASTE_FOLD_LINES: usize = 15;
/// Long one-line pastes should not make the editor visibly type character by
/// character. They are sent as a text attachment instead.
pub const PASTE_FOLD_CHARS: usize = 1_000;

/// The input's visual layout. The editor deliberately stores logical lines
/// only; this is the terminal-width-aware projection used for rendering.
pub struct EditorLayout {
    pub lines: Vec<EditorVisualLine>,
    pub cursor_row: usize,
    pub cursor_col: usize,
}

pub struct EditorVisualLine {
    pub first_logical_line: bool,
    pub text: String,
    pub logical_row: usize,
    pub start_col: usize,
    pub end_col: usize,
    pub selection: Option<(usize, usize)>,
}

/// Wrap logical editor lines ourselves instead of leaving it to the terminal.
/// That keeps soft wraps out of copied text, gives continuation lines a stable
/// prefix, and makes the cursor/viewport agree with what is on screen.
/// A display-width-bounded slice of a logical line. Tracks display columns
/// (for mapping mouse clicks back to a cursor position) and char offsets
/// (for slicing the selection highlight).
struct LayoutChunk {
    text: String,
    start_col: usize,
    end_col: usize,
    char_start: usize,
    char_end: usize,
}

pub enum VisualMove {
    Up,
    Down,
}

pub fn move_editor_visual(
    editor: &mut Editor,
    layout: &EditorLayout,
    direction: VisualMove,
) -> bool {
    let target_row = match direction {
        VisualMove::Up => match layout.cursor_row.checked_sub(1) {
            Some(row) => row,
            None => return false,
        },
        VisualMove::Down => {
            let row = layout.cursor_row + 1;
            if row >= layout.lines.len() {
                return false;
            }
            row
        }
    };

    let target = &layout.lines[target_row];
    let display_col = (target.start_col + layout.cursor_col).min(target.end_col);
    editor.set_cursor_by_display_col(target.logical_row, display_col);
    true
}

pub fn editor_layout(editor: &Editor, terminal_width: u16) -> EditorLayout {
    // border + two-column prompt + one interior column on the right.
    let width = terminal_width.saturating_sub(4).max(1) as usize;
    let (cursor_line, cursor_col) = editor.cursor();
    let selection = editor.selection_bounds();
    let mut lines = Vec::new();
    let mut visual_cursor = (0, 0);

    for (logical_row, text) in editor.lines().iter().enumerate() {
        let mut chunks: Vec<LayoutChunk> = Vec::new();
        let mut chunk = String::new();
        let mut start_col = 0usize;
        let mut end_col = 0usize;
        let mut char_start = 0usize;
        let mut char_index = 0usize;
        for c in text.chars() {
            let char_width = c.width().unwrap_or(0);
            if !chunk.is_empty() && end_col - start_col + char_width > width {
                chunks.push(LayoutChunk {
                    text: std::mem::take(&mut chunk),
                    start_col,
                    end_col,
                    char_start,
                    char_end: char_index,
                });
                start_col = end_col;
                char_start = char_index;
            }
            chunk.push(c);
            end_col += char_width;
            char_index += 1;
        }
        if !chunk.is_empty() || chunks.is_empty() {
            chunks.push(LayoutChunk {
                text: chunk,
                start_col,
                end_col,
                char_start,
                char_end: char_index,
            });
        }

        if logical_row == cursor_line {
            let cursor_chunk = chunks
                .iter()
                .rposition(|c| c.start_col <= cursor_col && cursor_col <= c.end_col)
                .unwrap_or(chunks.len() - 1);
            let start = chunks[cursor_chunk].start_col;
            visual_cursor = (lines.len() + cursor_chunk, cursor_col.saturating_sub(start));
        }
        for (i, chunk) in chunks.into_iter().enumerate() {
            let selection = selection.and_then(|(s, e)| {
                selection_span(logical_row, chunk.char_start, chunk.char_end, s, e)
            });
            lines.push(EditorVisualLine {
                first_logical_line: i == 0,
                text: chunk.text,
                logical_row,
                start_col: chunk.start_col,
                end_col: chunk.end_col,
                selection,
            });
        }
    }
    EditorLayout {
        lines,
        cursor_row: visual_cursor.0,
        cursor_col: visual_cursor.1,
    }
}

/// Build read-only ghost rows with the exact same wrapping rules as the editor.
/// Keeping this route shared is what makes a two-line suggestion grow the input
/// box instead of overflowing its first row.
pub fn ghost_visual_lines(text: &str, terminal_width: u16) -> Vec<String> {
    let mut ghost = Editor::new();
    ghost.insert_str(text);
    editor_layout(&ghost, terminal_width)
        .lines
        .into_iter()
        .map(|line| line.text)
        .collect()
}

/// Char range within a wrapped chunk `[char_start, char_end)` that falls
/// inside the selection `[start, end]` (both in logical row/char coords).
/// Returns offsets relative to the chunk, or `None` if disjoint.
pub fn selection_span(
    row: usize,
    char_start: usize,
    char_end: usize,
    start: Position,
    end: Position,
) -> Option<(usize, usize)> {
    if row < start.row || row > end.row {
        return None;
    }
    let sel_from = if row == start.row { start.col } else { 0 };
    let sel_to = if row == end.row { end.col } else { usize::MAX };
    let from = sel_from.max(char_start);
    let to = sel_to.min(char_end);
    (from < to).then(|| (from - char_start, to - char_start))
}

pub fn paste_should_fold(chars: usize, lines: usize) -> bool {
    chars > PASTE_FOLD_CHARS || lines > PASTE_FOLD_LINES
}

pub fn reference_boundary(chars: &[char], at: usize) -> bool {
    at == 0 || (!chars[at - 1].is_alphanumeric() && chars[at - 1] != '_')
}

pub fn reference_token_char(c: char) -> bool {
    !c.is_whitespace()
        && !matches!(
            c,
            '@' | '`' | '"' | '\'' | ')' | '(' | '[' | ']' | '{' | '}' | ',' | ';' | ':'
        )
}

/// Root-level project files are the least surprising completion targets, so
/// they outrank matching descendants before the usual relevance score applies.
pub fn reference_match_order(
    left_score: usize,
    left_path: &str,
    right_score: usize,
    right_path: &str,
) -> std::cmp::Ordering {
    left_path
        .contains('/')
        .cmp(&right_path.contains('/'))
        .then_with(|| left_score.cmp(&right_score))
        .then_with(|| left_path.cmp(right_path))
}

pub fn reference_score(path: &str, query: &str) -> Option<usize> {
    let path = path.to_lowercase();
    let query = query.to_lowercase();
    if query.is_empty() {
        return Some(0);
    }
    let basename = path.rsplit('/').next().unwrap_or(&path);
    if basename.starts_with(&query) {
        return Some(0);
    }
    if path.starts_with(&query) {
        return Some(1);
    }
    let mut next = 0;
    let mut gaps = 0;
    for wanted in query.chars() {
        let found = path[next..].find(wanted)?;
        gaps += found;
        next += found + wanted.len_utf8();
    }
    Some(10 + gaps)
}

pub fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    }
}

pub fn reference_candidate_path(candidate: &ReferenceCandidate) -> String {
    match candidate.kind {
        ReferenceKind::Directory => format!("{}/", candidate.path),
        ReferenceKind::File => candidate.path.clone(),
    }
}

pub fn reference_basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

pub fn reference_basename_path(candidate: &ReferenceCandidate) -> String {
    let mut name = reference_basename(&candidate.path).to_string();
    if matches!(candidate.kind, ReferenceKind::Directory) {
        name.push('/');
    }
    name
}

pub fn reference_marker(path: &str) -> String {
    if path.chars().any(char::is_whitespace) {
        format!("@\"{path}\"")
    } else {
        format!("@{path}")
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum InputSpanStyle {
    Plain,
    Token,
    Selection,
}

/// Style exact project references and attachment placeholders directly in the
/// input, while preserving selection highlighting as the higher-priority
/// interaction state. An unrecognized `@token` remains ordinary prose.
pub fn input_spans(
    text: &str,
    selection: Option<(usize, usize)>,
    references: &[ReferenceCandidate],
) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let token_ranges = input_token_ranges(&chars, references);
    let style_at = |index| {
        if selection.is_some_and(|(from, to)| from <= index && index < to) {
            InputSpanStyle::Selection
        } else if token_ranges
            .iter()
            .any(|&(from, to)| from <= index && index < to)
        {
            InputSpanStyle::Token
        } else {
            InputSpanStyle::Plain
        }
    };

    let mut spans = Vec::new();
    let mut start = 0;
    let mut style = style_at(0);
    for index in 1..=chars.len() {
        let next_style = (index < chars.len()).then(|| style_at(index));
        if next_style == Some(style) {
            continue;
        }
        let segment: String = chars[start..index].iter().collect();
        let span = match style {
            InputSpanStyle::Plain => Span::raw(segment),
            InputSpanStyle::Token => Span::styled(segment, theme::accent()),
            InputSpanStyle::Selection => Span::styled(segment, theme::selection()),
        };
        spans.push(span);
        start = index;
        if let Some(next_style) = next_style {
            style = next_style;
        }
    }
    spans
}

pub fn input_token_ranges(
    chars: &[char],
    references: &[ReferenceCandidate],
) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '@' && reference_boundary(chars, index) {
            let mut end = index + 1;
            if chars.get(end) == Some(&'"') {
                end += 1;
                while end < chars.len() && chars[end] != '"' {
                    end += 1;
                }
                if end < chars.len() {
                    end += 1;
                }
            } else {
                while end < chars.len() && reference_token_char(chars[end]) {
                    end += 1;
                }
            }
            if known_reference_marker(chars, index, end, references) {
                ranges.push((index, end));
            }
            index = end;
            continue;
        }
        let attachment_end = ["[Image #", "[Pasted text #"].iter().find_map(|prefix| {
            let prefix_len = prefix.chars().count();
            (chars[index..]
                .iter()
                .take(prefix_len)
                .copied()
                .eq(prefix.chars()))
            .then(|| {
                let mut end = index + prefix_len;
                while end < chars.len() && chars[end].is_ascii_digit() {
                    end += 1;
                }
                (end > index + prefix_len && chars.get(end) == Some(&']')).then_some(end + 1)
            })
            .flatten()
        });
        if let Some(end) = attachment_end {
            ranges.push((index, end));
            index = end;
        } else {
            index += 1;
        }
    }
    ranges
}

pub fn known_reference_marker(
    chars: &[char],
    start: usize,
    end: usize,
    references: &[ReferenceCandidate],
) -> bool {
    let marker: String = chars[start..end].iter().collect();
    let Some(raw) = marker
        .strip_prefix("@\"")
        .and_then(|quoted| quoted.strip_suffix('"'))
        .or_else(|| marker.strip_prefix('@'))
    else {
        return false;
    };
    references.iter().any(|candidate| match candidate.kind {
        ReferenceKind::File => raw == candidate.path,
        ReferenceKind::Directory => raw.trim_end_matches('/') == candidate.path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_or_multiline_pastes_fold_into_attachments() {
        assert!(!paste_should_fold(PASTE_FOLD_CHARS, 1));
        assert!(paste_should_fold(PASTE_FOLD_CHARS + 1, 1));
        assert!(!paste_should_fold(1, PASTE_FOLD_LINES));
        assert!(paste_should_fold(1, PASTE_FOLD_LINES + 1));
    }

    #[test]
    fn ghost_suggestion_uses_the_editor_wrap_width() {
        // Width 10 leaves six input cells after the border and prompt gutter.
        assert_eq!(ghost_visual_lines("abcdefghi", 10), ["abcdef", "ghi"]);
    }

    #[test]
    fn editor_layout_wraps_without_losing_cursor_position() {
        let mut editor = Editor::new();
        editor.insert_str("abcdefghi");
        // Width 10 leaves six cells inside the input border and prompt.
        let layout = editor_layout(&editor, 10);
        assert_eq!(
            layout
                .lines
                .iter()
                .map(|vl| vl.text.as_str())
                .collect::<Vec<_>>(),
            ["abcdef", "ghi"]
        );
        assert_eq!((layout.cursor_row, layout.cursor_col), (1, 3));
    }

    #[test]
    fn editor_layout_places_boundary_cursor_on_next_soft_wrap() {
        let mut editor = Editor::new();
        editor.insert_str("abcdefghi");
        editor.set_cursor(0, 6);
        let layout = editor_layout(&editor, 10);
        assert_eq!((layout.cursor_row, layout.cursor_col), (1, 0));
    }

    #[test]
    fn editor_visual_move_crosses_soft_wrapped_lines() {
        let mut editor = Editor::new();
        editor.insert_str("abcdefghi");
        // Width 10 leaves six cells inside the input border and prompt:
        // visual rows are "abcdef" and "ghi".
        let layout = editor_layout(&editor, 10);
        assert_eq!((layout.cursor_row, layout.cursor_col), (1, 3));

        assert!(move_editor_visual(&mut editor, &layout, VisualMove::Up));
        assert_eq!(editor.position(), Position { row: 0, col: 3 });

        let layout = editor_layout(&editor, 10);
        assert!(move_editor_visual(&mut editor, &layout, VisualMove::Down));
        assert_eq!(editor.position(), Position { row: 0, col: 9 });
    }

    #[test]
    fn editor_layout_marks_selection_across_a_soft_wrap() {
        let mut editor = Editor::new();
        editor.insert_str("abcdefghi");
        // Select chars 4..8 ("efgh"), which straddles the wrap at 6.
        editor.set_cursor(0, 4);
        editor.start_selection_by_display_col(0, 4);
        editor.extend_selection_by_display_col(0, 8);
        let layout = editor_layout(&editor, 10);
        // First visual line "abcdef": tail "ef" (offsets 4..6) selected.
        assert_eq!(layout.lines[0].selection, Some((4, 6)));
        // Second visual line "ghi": head "gh" (offsets 0..2) selected.
        assert_eq!(layout.lines[1].selection, Some((0, 2)));
    }

    #[test]
    fn editor_layout_keeps_explicit_newlines_distinct_from_soft_wraps() {
        let mut editor = Editor::new();
        editor.insert_str("abc\ndef");
        let layout = editor_layout(&editor, 10);
        assert_eq!(
            layout
                .lines
                .iter()
                .map(|vl| (vl.first_logical_line, vl.text.as_str()))
                .collect::<Vec<_>>(),
            [(true, "abc"), (true, "def")]
        );
    }

    #[test]
    fn input_tokens_accent_only_known_references_and_attachment_placeholders() {
        let references = [ReferenceCandidate {
            path: "src/app.rs".into(),
            kind: ReferenceKind::File,
            bytes: Some(1),
        }];
        let spans = input_spans(
            "see @src/app.rs, but @not-a-file [Image #2] [Pasted text #3] and me@example.com",
            None,
            &references,
        );
        let accented: Vec<_> = spans
            .iter()
            .filter(|span| span.style.fg == Some(theme::ACCENT))
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(accented, ["@src/app.rs", "[Image #2]", "[Pasted text #3]"]);
        let plain: String = spans
            .iter()
            .filter(|span| span.style.fg.is_none())
            .map(|span| span.content.as_ref())
            .collect();
        assert!(
            plain.contains("@not-a-file"),
            "unmatched at-sign text is ordinary prose"
        );
    }

    #[test]
    fn selection_overrides_input_token_accent() {
        let references = [ReferenceCandidate {
            path: "src/app.rs".into(),
            kind: ReferenceKind::File,
            bytes: Some(1),
        }];
        let spans = input_spans("@src/app.rs", Some((0, 11)), &references);
        assert_eq!(spans.len(), 1);
        assert_ne!(spans[0].style.fg, Some(theme::ACCENT));
    }

    #[test]
    fn reference_labels_use_basenames_unless_they_conflict() {
        let file = ReferenceCandidate {
            path: "crates/tcode-tui/src/app.rs".into(),
            kind: ReferenceKind::File,
            bytes: Some(1),
        };
        let directory = ReferenceCandidate {
            path: "crates/tcode-tui/src".into(),
            kind: ReferenceKind::Directory,
            bytes: None,
        };
        assert_eq!(reference_basename_path(&file), "app.rs");
        assert_eq!(reference_basename_path(&directory), "src/");
        assert_eq!(
            reference_marker(&reference_candidate_path(&file)),
            "@crates/tcode-tui/src/app.rs"
        );
    }

    #[test]
    fn reference_matching_prefers_basenames_then_fuzzy_paths() {
        assert_eq!(
            reference_score("crates/tcode-tui/src/app.rs", "app"),
            Some(0)
        );
        assert_eq!(
            reference_score("crates/tcode-tui/src/app.rs", "crates"),
            Some(1)
        );
        assert!(reference_score("crates/tcode-tui/src/app.rs", "tuiapp").is_some());
        assert!(reference_score("crates/tcode-tui/src/app.rs", "xyz").is_none());
    }

    #[test]
    fn reference_matching_prioritizes_root_files() {
        let mut matches = vec![(0, "src/Cargo.toml"), (10, "Cargo.toml"), (0, "README.md")];
        matches.sort_by(|(left_score, left), (right_score, right)| {
            reference_match_order(*left_score, left, *right_score, right)
        });
        assert_eq!(
            matches,
            [(0, "README.md"), (10, "Cargo.toml"), (0, "src/Cargo.toml")]
        );
    }

    #[test]
    fn reference_token_avoids_email_addresses() {
        let email: Vec<char> = "me@example.com".chars().collect();
        assert!(!reference_boundary(&email, 2));
        let mention: Vec<char> = "read @src".chars().collect();
        assert!(reference_boundary(&mention, 5));
    }
}
