/// Token budget gate for tool outputs. Anything over budget goes to the
/// blob store; the context gets head+tail preview plus a paging handle
/// (`read_output` tool). A 50KB build log never floods the context.
///
/// In-memory for now; the persistence layer (M3) swaps storage without
/// changing this API.
#[derive(Debug)]
pub struct BlobStore {
    blobs: Vec<Blob>,
    budget_tokens: usize,
}

#[derive(Debug)]
struct Blob {
    tool: String,
    text: String,
}

/// Rough token estimate good enough for budgeting (code/mixed text).
pub fn approx_tokens(s: &str) -> usize {
    s.chars().count().div_ceil(3)
}

impl BlobStore {
    pub fn new(budget_tokens: usize) -> Self {
        Self {
            blobs: Vec::new(),
            budget_tokens,
        }
    }

    /// Pass small outputs through; store large ones and return a preview
    /// with a handle.
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

        let id = format!("o{}", self.blobs.len() + 1);
        self.blobs.push(Blob {
            tool: tool.to_string(),
            text: full.clone(),
        });

        let mut out = String::new();
        for l in &lines[..head.len().min(tail_start)] {
            out.push_str(l);
            out.push('\n');
        }
        let hidden = tail_start.saturating_sub(head.len());
        if hidden > 0 {
            out.push_str(&format!(
                "\n… [{hidden} lines omitted — full output saved as {id}; \
                 use read_output(id=\"{id}\", offset=N) to page] …\n\n"
            ));
        }
        for l in &lines[tail_start.max(head.len())..] {
            out.push_str(l);
            out.push('\n');
        }
        out.push_str(&format!(
            "\n[output truncated: {total_lines} lines total, id={id}]"
        ));
        out
    }

    /// Page through a stored blob. 1-based offset, in lines.
    pub fn read(&self, id: &str, offset: usize, limit: usize) -> Result<String, String> {
        let index: usize = id
            .strip_prefix('o')
            .and_then(|n| n.parse::<usize>().ok())
            .filter(|n| (1..=self.blobs.len()).contains(n))
            .ok_or_else(|| {
                format!(
                    "unknown output id '{id}'. Available: {}",
                    if self.blobs.is_empty() {
                        "none".to_string()
                    } else {
                        (1..=self.blobs.len())
                            .map(|i| format!("o{i} ({})", self.blobs[i - 1].tool))
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                )
            })?;
        let blob = &self.blobs[index - 1];
        let lines: Vec<&str> = blob.text.lines().collect();
        let start = offset.saturating_sub(1).min(lines.len());
        let end = start.saturating_add(limit).min(lines.len());
        let mut out = String::new();
        for (i, l) in lines[start..end].iter().enumerate() {
            out.push_str(&format!("{:>6}\t{l}\n", start + i + 1));
        }
        if end < lines.len() {
            out.push_str(&format!(
                "[{} more lines; continue with offset={}]",
                lines.len() - end,
                end + 1
            ));
        }
        Ok(out)
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

    #[test]
    fn small_output_passes_through() {
        let mut store = BlobStore::new(100);
        let s = "hello\nworld".to_string();
        assert_eq!(store.gate("shell", s.clone()), s);
    }

    #[test]
    fn large_output_is_gated_and_pageable() {
        let mut store = BlobStore::new(50); // ~150 chars
        let full: String = (1..=100).map(|i| format!("line number {i}\n")).collect();
        let gated = store.gate("shell", full);
        assert!(gated.contains("id=o1"));
        assert!(gated.len() < 1000);
        assert!(gated.contains("line number 1")); // head kept
        assert!(gated.contains("line number 100")); // tail kept

        let page = store.read("o1", 40, 5).unwrap();
        assert!(page.contains("line number 40"));
        assert!(page.contains("line number 44"));
        assert!(page.contains("continue with offset=45"));
    }

    #[test]
    fn unknown_id_lists_available() {
        let store = BlobStore::new(10);
        let err = store.read("o9", 1, 10).unwrap_err();
        assert!(err.contains("none"));
    }
}
