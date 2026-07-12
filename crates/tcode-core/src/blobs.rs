use std::path::PathBuf;

/// Token budget gate for tool outputs. Anything over budget is written to a
/// file under the project's `tool-output/` dir; the context gets a head+tail
/// preview plus the path, which the model pages with the normal `read`/`grep`
/// tools. A 50 KB build log never floods the context, and there is no bespoke
/// paging tool to learn — an overflow is just a file.
#[derive(Debug)]
pub struct BlobStore {
    dir: PathBuf,
    budget_tokens: usize,
    counter: usize,
}

/// Rough token estimate good enough for budgeting (code/mixed text).
pub fn approx_tokens(s: &str) -> usize {
    s.chars().count().div_ceil(3)
}

impl BlobStore {
    pub fn new(dir: PathBuf, budget_tokens: usize) -> Self {
        Self {
            dir,
            budget_tokens,
            counter: 0,
        }
    }

    /// Pass small outputs through; spill large ones to a file and return a
    /// head+tail preview with the saved path.
    pub fn gate(&mut self, tool: &str, full: String) -> String {
        if approx_tokens(&full) <= self.budget_tokens {
            return full;
        }
        let lines: Vec<&str> = full.lines().collect();
        let total_lines = lines.len();
        // Head shows the beginning, tail shows the end (build failures
        // usually live at the end).
        let budget_chars = self.budget_tokens * 3;
        let head = take_chars(lines.iter().copied(), budget_chars * 3 / 5);
        let tail_count = take_chars(lines.iter().rev().copied(), budget_chars / 5).len();
        let tail_start = total_lines - tail_count;

        self.counter += 1;
        let saved = self.write_overflow(tool, &full);

        let mut out = String::new();
        for l in &lines[..head.len().min(tail_start)] {
            out.push_str(l);
            out.push('\n');
        }
        let hidden = tail_start.saturating_sub(head.len());
        if hidden > 0 {
            match &saved {
                Some(path) => out.push_str(&format!(
                    "\n… [{hidden} lines omitted — full output saved to {path}; \
                     read or grep that file to see the rest] …\n\n"
                )),
                None => out.push_str(&format!(
                    "\n… [{hidden} lines omitted — full output too large to save] …\n\n"
                )),
            }
        }
        for l in &lines[tail_start.max(head.len())..] {
            out.push_str(l);
            out.push('\n');
        }
        match saved {
            Some(path) => out.push_str(&format!(
                "\n[output truncated: {total_lines} lines total, full copy at {path}]"
            )),
            None => out.push_str(&format!("\n[output truncated: {total_lines} lines total]")),
        }
        out
    }

    /// Write the full output to `tool-output/NNN-<tool>.txt`, creating the
    /// directory lazily. Returns the path as a string, or None if it could not
    /// be written (the preview is still returned either way).
    fn write_overflow(&self, tool: &str, full: &str) -> Option<String> {
        std::fs::create_dir_all(&self.dir).ok()?;
        let safe: String = tool
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '_' })
            .collect();
        let path = self.dir.join(format!("{:03}-{safe}.txt", self.counter));
        std::fs::write(&path, full).ok()?;
        Some(path.display().to_string())
    }
}

/// How many leading lines fit within a char budget.
fn take_chars<'a>(lines: impl Iterator<Item = &'a str>, budget: usize) -> Vec<&'a str> {
    let mut used = 0;
    let mut out = Vec::new();
    for l in lines {
        used += l.chars().count() + 1;
        if used > budget {
            break;
        }
        out.push(l);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(budget: usize) -> BlobStore {
        let dir = std::env::temp_dir().join(format!("tcode-blob-{}", std::process::id()));
        BlobStore::new(dir, budget)
    }

    #[test]
    fn small_output_passes_through() {
        let mut store = store(100);
        let s = "hello\nworld".to_string();
        assert_eq!(store.gate("shell", s.clone()), s);
    }

    #[test]
    fn large_output_spills_to_a_file() {
        let mut store = store(50); // ~150 chars
        let full: String = (1..=100).map(|i| format!("line number {i}\n")).collect();
        let gated = store.gate("shell", full.clone());

        assert!(gated.len() < 1000);
        assert!(gated.contains("line number 1")); // head kept
        assert!(gated.contains("line number 100")); // tail kept
        assert!(gated.contains("full output saved to"));
        assert!(gated.contains("-shell.txt"));

        // The saved file holds the complete output for read/grep to page.
        let path = gated
            .lines()
            .find_map(|l| l.strip_prefix("[output truncated: 100 lines total, full copy at "))
            .map(|s| s.trim_end_matches(']'))
            .unwrap();
        let saved = std::fs::read_to_string(path).unwrap();
        assert_eq!(saved, full);
        let _ = std::fs::remove_file(path);
    }
}
