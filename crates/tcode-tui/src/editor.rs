//! Minimal multi-line input editor: enough for a comfortable prompt
//! without pulling in a textarea dependency.

use unicode_width::UnicodeWidthChar;

#[derive(Debug, Default)]
pub struct Editor {
    /// Lines of input (always at least one).
    lines: Vec<String>,
    row: usize,
    /// Column in chars (not bytes).
    col: usize,
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
        let width: usize = self.lines[self.row]
            .chars()
            .take(self.col)
            .map(|c| c.width().unwrap_or(0))
            .sum();
        (self.row, width)
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn insert_char(&mut self, c: char) {
        let byte = char_to_byte(&self.lines[self.row], self.col);
        self.lines[self.row].insert(byte, c);
        self.col += 1;
        self.history_pos = None;
    }

    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars() {
            if c == '\n' {
                self.newline();
            } else if c != '\r' {
                self.insert_char(c);
            }
        }
    }

    pub fn newline(&mut self) {
        let byte = char_to_byte(&self.lines[self.row], self.col);
        let rest = self.lines[self.row].split_off(byte);
        self.lines.insert(self.row + 1, rest);
        self.row += 1;
        self.col = 0;
    }

    pub fn backspace(&mut self) {
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
    }

    pub fn delete(&mut self) {
        let len = self.lines[self.row].chars().count();
        if self.col < len {
            let byte = char_to_byte(&self.lines[self.row], self.col);
            self.lines[self.row].remove(byte);
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
    }

    pub fn left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.lines[self.row].chars().count();
        }
    }

    pub fn right(&mut self) {
        if self.col < self.lines[self.row].chars().count() {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    /// Returns false when already on the first row (caller may use the
    /// key for history navigation instead).
    pub fn up(&mut self) -> bool {
        if self.row == 0 {
            return false;
        }
        self.row -= 1;
        self.col = self.col.min(self.lines[self.row].chars().count());
        true
    }

    pub fn down(&mut self) -> bool {
        if self.row + 1 >= self.lines.len() {
            return false;
        }
        self.row += 1;
        self.col = self.col.min(self.lines[self.row].chars().count());
        true
    }

    pub fn home(&mut self) {
        self.col = 0;
    }

    pub fn end(&mut self) {
        self.col = self.lines[self.row].chars().count();
    }

    pub fn clear(&mut self) {
        self.lines = vec![String::new()];
        self.row = 0;
        self.col = 0;
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

    fn set_text(&mut self, text: &str) {
        self.lines = text.split('\n').map(String::from).collect();
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.row = self.lines.len() - 1;
        self.col = self.lines[self.row].chars().count();
    }
}

fn char_to_byte(s: &str, col: usize) -> usize {
    s.char_indices()
        .nth(col)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
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
    fn unicode_safe() {
        let mut e = Editor::new();
        e.insert_str("中文测试");
        e.left();
        e.backspace();
        assert_eq!(e.text(), "中文试");
        e.insert_char('好');
        assert_eq!(e.text(), "中文好试");
    }
}
