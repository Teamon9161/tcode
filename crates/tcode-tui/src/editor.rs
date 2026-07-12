//! Minimal multi-line input editor: enough for a comfortable prompt
//! without pulling in a textarea dependency.

use unicode_width::UnicodeWidthChar;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct Position {
    pub row: usize,
    /// Column in chars (not bytes).
    pub col: usize,
}

#[derive(Debug, Default)]
pub struct Editor {
    /// Lines of input (always at least one).
    lines: Vec<String>,
    row: usize,
    /// Column in chars (not bytes).
    col: usize,
    selection_anchor: Option<Position>,
    history: Vec<String>,
    history_pos: Option<usize>,
    stash: String,
}

impl Editor {
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            ..Default::default()
        }
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// (row, col) for cursor placement, col in display width.
    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.display_col(self.row, self.col))
    }

    pub fn position(&self) -> Position {
        Position {
            row: self.row,
            col: self.col,
        }
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn insert_char(&mut self, c: char) {
        self.delete_selection();
        let byte = char_to_byte(&self.lines[self.row], self.col);
        self.lines[self.row].insert(byte, c);
        self.col += 1;
        self.history_pos = None;
    }

    pub fn insert_str(&mut self, s: &str) {
        self.delete_selection();
        let row = self.row;
        let byte = char_to_byte(&self.lines[row], self.col);
        let tail = self.lines[row].split_off(byte);
        let mut pasted = s.split('\n');
        let first = pasted.next().unwrap_or_default();
        let first = first.strip_suffix('\r').unwrap_or(first);
        self.lines[row].push_str(first);

        let mut inserted_rows = 0usize;
        for fragment in pasted {
            inserted_rows += 1;
            self.lines.insert(
                row + inserted_rows,
                fragment.strip_suffix('\r').unwrap_or(fragment).to_string(),
            );
        }
        let last = row + inserted_rows;
        self.lines[last].push_str(&tail);
        self.row = last;
        self.col = self.lines[last]
            .chars()
            .count()
            .saturating_sub(tail.chars().count());
        self.history_pos = None;
    }

    pub fn newline(&mut self) {
        self.delete_selection();
        let byte = char_to_byte(&self.lines[self.row], self.col);
        let rest = self.lines[self.row].split_off(byte);
        self.lines.insert(self.row + 1, rest);
        self.row += 1;
        self.col = 0;
        self.history_pos = None;
    }

    pub fn backspace(&mut self) {
        if self.delete_selection() {
            return;
        }
        if self.col > 0 {
            let byte = char_to_byte(&self.lines[self.row], self.col - 1);
            self.lines[self.row].remove(byte);
            self.col -= 1;
        } else if self.row > 0 {
            let cur = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.lines[self.row].chars().count();
            self.lines[self.row].push_str(&cur);
        }
        self.history_pos = None;
    }

    pub fn delete(&mut self) {
        if self.delete_selection() {
            return;
        }
        let len = self.lines[self.row].chars().count();
        if self.col < len {
            let byte = char_to_byte(&self.lines[self.row], self.col);
            self.lines[self.row].remove(byte);
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
        self.history_pos = None;
    }

    pub fn left(&mut self) {
        self.clear_selection();
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.lines[self.row].chars().count();
        }
    }

    pub fn right(&mut self) {
        self.clear_selection();
        if self.col < self.lines[self.row].chars().count() {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    pub fn home(&mut self) {
        self.clear_selection();
        self.col = 0;
    }

    pub fn end(&mut self) {
        self.clear_selection();
        self.col = self.lines[self.row].chars().count();
    }

    pub fn clear(&mut self) {
        self.lines = vec![String::new()];
        self.row = 0;
        self.col = 0;
        self.selection_anchor = None;
        self.history_pos = None;
    }

    /// Submit: push to history and reset.
    pub fn take(&mut self) -> String {
        let text = self.text();
        if !text.trim().is_empty() {
            self.history.push(text.clone());
        }
        self.clear();
        text
    }

    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let pos = match self.history_pos {
            None => {
                self.stash = self.text();
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(p) => p - 1,
        };
        self.set_text(&self.history[pos].clone());
        self.history_pos = Some(pos);
    }

    pub fn history_next(&mut self) {
        match self.history_pos {
            None => {}
            Some(p) if p + 1 < self.history.len() => {
                self.set_text(&self.history[p + 1].clone());
                self.history_pos = Some(p + 1);
            }
            Some(_) => {
                let stash = self.stash.clone();
                self.set_text(&stash);
                self.history_pos = None;
            }
        }
    }

    pub fn set_cursor(&mut self, row: usize, col: usize) {
        let row = row.min(self.lines.len().saturating_sub(1));
        self.row = row;
        self.col = col.min(self.lines[row].chars().count());
        self.selection_anchor = None;
    }

    pub fn set_cursor_by_display_col(&mut self, row: usize, display_col: usize) {
        let row = row.min(self.lines.len().saturating_sub(1));
        let col = display_to_char_col(&self.lines[row], display_col);
        self.set_cursor(row, col);
    }

    pub fn start_selection_by_display_col(&mut self, row: usize, display_col: usize) {
        self.set_cursor_by_display_col(row, display_col);
        self.selection_anchor = Some(self.position());
    }

    pub fn extend_selection_by_display_col(&mut self, row: usize, display_col: usize) {
        let row = row.min(self.lines.len().saturating_sub(1));
        self.row = row;
        self.col = display_to_char_col(&self.lines[row], display_col);
        if self.selection_anchor.is_none() {
            self.selection_anchor = Some(self.position());
        }
    }

    pub fn select_all(&mut self) {
        self.selection_anchor = Some(Position { row: 0, col: 0 });
        self.row = self.lines.len() - 1;
        self.col = self.lines[self.row].chars().count();
    }

    pub fn selected_text(&self) -> Option<String> {
        let (start, end) = self.selection_bounds()?;
        let mut out = String::new();
        for row in start.row..=end.row {
            if row > start.row {
                out.push('\n');
            }
            let line = &self.lines[row];
            let from = if row == start.row { start.col } else { 0 };
            let to = if row == end.row {
                end.col
            } else {
                line.chars().count()
            };
            out.push_str(&line_slice(line, from, to));
        }
        Some(out)
    }

    pub fn selection_bounds(&self) -> Option<(Position, Position)> {
        let anchor = self.selection_anchor?;
        let head = self.position();
        if anchor == head {
            return None;
        }
        Some(if anchor <= head {
            (anchor, head)
        } else {
            (head, anchor)
        })
    }

    pub fn display_col(&self, row: usize, col: usize) -> usize {
        self.lines
            .get(row)
            .map(|line| line.chars().take(col).map(|c| c.width().unwrap_or(0)).sum())
            .unwrap_or(0)
    }

    fn set_text(&mut self, text: &str) {
        self.lines = text.split('\n').map(String::from).collect();
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.row = self.lines.len() - 1;
        self.col = self.lines[self.row].chars().count();
        self.selection_anchor = None;
    }

    fn clear_selection(&mut self) {
        self.selection_anchor = None;
    }

    pub fn delete_selection(&mut self) -> bool {
        let Some((start, end)) = self.selection_bounds() else {
            self.selection_anchor = None;
            return false;
        };
        if start.row == end.row {
            let line = &mut self.lines[start.row];
            let from = char_to_byte(line, start.col);
            let to = char_to_byte(line, end.col);
            line.replace_range(from..to, "");
        } else {
            let prefix = {
                let line = &self.lines[start.row];
                line[..char_to_byte(line, start.col)].to_string()
            };
            let suffix = {
                let line = &self.lines[end.row];
                line[char_to_byte(line, end.col)..].to_string()
            };
            self.lines
                .splice(start.row..=end.row, [format!("{prefix}{suffix}")]);
        }
        self.row = start.row;
        self.col = start.col;
        self.selection_anchor = None;
        self.history_pos = None;
        true
    }
}

fn char_to_byte(s: &str, col: usize) -> usize {
    s.char_indices().nth(col).map(|(i, _)| i).unwrap_or(s.len())
}

fn display_to_char_col(s: &str, display_col: usize) -> usize {
    let mut width = 0usize;
    for (i, c) in s.chars().enumerate() {
        let char_width = c.width().unwrap_or(0);
        if display_col <= width + char_width / 2 {
            return i;
        }
        width += char_width;
        if display_col < width {
            return i + 1;
        }
    }
    s.chars().count()
}

fn line_slice(s: &str, from: usize, to: usize) -> String {
    s.chars().skip(from).take(to.saturating_sub(from)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_and_submit() {
        let mut e = Editor::new();
        e.insert_str("hello");
        e.newline();
        e.insert_str("world");
        assert_eq!(e.text(), "hello\nworld");
        e.backspace();
        assert_eq!(e.text(), "hello\nworl");
        assert_eq!(e.take(), "hello\nworl");
        assert!(e.is_empty());
        e.history_prev();
        assert_eq!(e.text(), "hello\nworl");
    }

    #[test]
    fn bulk_insert_keeps_tail_and_cursor_position() {
        let mut e = Editor::new();
        e.insert_str("headtail");
        e.home();
        for _ in 0..4 {
            e.right();
        }
        e.insert_str("A\nB");
        assert_eq!(e.text(), "headA\nBtail");
        assert_eq!(e.position(), Position { row: 1, col: 1 });
    }

    #[test]
    fn unicode_safe() {
        let mut e = Editor::new();
        e.insert_str("中文测试");
        e.left();
        e.backspace();
        assert_eq!(e.text(), "中文试");
        e.insert_char('好');
        assert_eq!(e.text(), "中文好试");
    }

    #[test]
    fn selection_copy_delete_and_replace() {
        let mut e = Editor::new();
        e.insert_str("hello\nworld");
        e.select_all();
        assert_eq!(e.selected_text().as_deref(), Some("hello\nworld"));
        e.insert_str("ok");
        assert_eq!(e.text(), "ok");

        e.select_all();
        assert!(e.delete_selection());
        assert!(e.is_empty());
    }

    #[test]
    fn mouse_display_columns_are_unicode_aware() {
        let mut e = Editor::new();
        e.insert_str("a中b");
        e.set_cursor_by_display_col(0, 3);
        assert_eq!(e.position(), Position { row: 0, col: 2 });
    }
}
