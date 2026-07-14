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
        // A head+tail cut of a diff hides the one thing the reader needs first
        // — which files changed. We still have the whole text here, so answer
        // that before throwing the middle away, instead of making the model
        // grep the spill file back for it.
        if let Some(summary) = diff_summary(&full) {
            out.push_str(&summary);
            out.push('\n');
        }
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

/// At most this many files are listed before the rest are rolled into a count;
/// the preamble must stay a preamble, not become the output.
const MAX_LISTED_FILES: usize = 50;

#[derive(Default)]
struct FileDiff {
    path: String,
    added: usize,
    removed: usize,
    new_file: bool,
    deleted: bool,
    /// The `+++` header for this file has been consumed. A plain `diff -u` has
    /// no `diff --git` line, so its `+++` is what starts a file; in git output
    /// the header already started it and the `+++` only restates the path.
    saw_plus_header: bool,
}

/// Per-file `+`/`-` counts for a unified diff, or None if this is not one.
/// Handles `git diff`/`git show` output and plain `diff -u`.
fn diff_summary(full: &str) -> Option<String> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut saw_git_header = false;
    let mut saw_hunk = false;

    for line in full.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            saw_git_header = true;
            files.push(FileDiff {
                path: git_header_path(rest),
                ..Default::default()
            });
        } else if line.starts_with("new file mode") {
            if let Some(f) = files.last_mut() {
                f.new_file = true;
            }
        } else if line.starts_with("deleted file mode") {
            if let Some(f) = files.last_mut() {
                f.deleted = true;
            }
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            let path = strip_diff_prefix(rest);
            match files.last_mut() {
                Some(f) if !f.saw_plus_header => {
                    f.saw_plus_header = true;
                    if f.path.is_empty() {
                        f.path = path;
                    }
                }
                _ => files.push(FileDiff {
                    path,
                    saw_plus_header: true,
                    ..Default::default()
                }),
            }
        } else if line.starts_with("--- ") {
            // Header, not a removed line.
        } else if line.starts_with("@@ ") {
            saw_hunk = true;
        } else if let Some(f) = files.last_mut() {
            if line.starts_with('+') {
                f.added += 1;
            } else if line.starts_with('-') {
                f.removed += 1;
            }
        }
    }

    // Prose that happens to contain a `+++` line is not a diff.
    if files.is_empty() || !(saw_git_header || saw_hunk) {
        return None;
    }

    let total_added: usize = files.iter().map(|f| f.added).sum();
    let total_removed: usize = files.iter().map(|f| f.removed).sum();
    let shown = files.len().min(MAX_LISTED_FILES);
    let width = files[..shown]
        .iter()
        .map(|f| f.path.chars().count())
        .max()
        .unwrap_or(0)
        .min(60);

    let mut out = format!(
        "[truncated diff — {} file{} changed, +{total_added} -{total_removed}]\n",
        files.len(),
        if files.len() == 1 { "" } else { "s" },
    );
    for f in &files[..shown] {
        let tag = if f.new_file {
            "  (new)"
        } else if f.deleted {
            "  (deleted)"
        } else {
            ""
        };
        out.push_str(&format!(
            "  {:<width$}  +{} -{}{tag}\n",
            f.path,
            f.added,
            f.removed,
            width = width
        ));
    }
    if files.len() > shown {
        out.push_str(&format!("  … and {} more files\n", files.len() - shown));
    }
    Some(out)
}

/// `a/src/foo.rs b/src/foo.rs` -> `src/foo.rs`, preferring the post-image name
/// so renames report where the content ended up.
fn git_header_path(rest: &str) -> String {
    rest.split_whitespace()
        .next_back()
        .map(strip_diff_prefix)
        .unwrap_or_default()
}

/// Drop the `a/` / `b/` prefix and any trailing tab-separated timestamp that
/// plain `diff -u` appends.
fn strip_diff_prefix(s: &str) -> String {
    let s = s.split('\t').next().unwrap_or(s).trim();
    s.strip_prefix("a/")
        .or_else(|| s.strip_prefix("b/"))
        .unwrap_or(s)
        .to_string()
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

    /// Overflow files are named by a per-store counter, so two stores sharing a
    /// directory would overwrite each other's spill under a parallel test run.
    fn store(budget: usize) -> BlobStore {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("tcode-blob-{}-{n}", std::process::id()));
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

    const GIT_DIFF: &str = "\
diff --git a/src/one.rs b/src/one.rs
index 1111111..2222222 100644
--- a/src/one.rs
+++ b/src/one.rs
@@ -1,3 +1,4 @@
 fn main() {
-    old();
+    new();
+    more();
 }
diff --git a/src/two.rs b/src/two.rs
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/src/two.rs
@@ -0,0 +1,2 @@
+fn two() {}
+// added
diff --git a/src/gone.rs b/src/gone.rs
deleted file mode 100644
index 4444444..0000000
--- a/src/gone.rs
+++ /dev/null
@@ -1,1 +0,0 @@
-fn gone() {}
";

    #[test]
    fn diff_summary_counts_changes_per_file() {
        let summary = diff_summary(GIT_DIFF).unwrap();
        // Headers (`+++`, `---`, `@@`) are not counted as changed lines.
        assert!(summary.contains("3 files changed, +4 -2"));
        assert!(summary.contains("src/one.rs"));
        assert!(summary.contains("+2 -1"));
        assert!(summary.contains("src/two.rs"));
        assert!(summary.contains("(new)"));
        assert!(summary.contains("src/gone.rs"));
        assert!(summary.contains("(deleted)"));
    }

    #[test]
    fn diff_summary_reads_plain_unified_diffs() {
        let plain = "\
--- old.txt\t2026-07-14
+++ new.txt\t2026-07-14
@@ -1,2 +1,2 @@
-before
+after
";
        let summary = diff_summary(plain).unwrap();
        assert!(summary.contains("1 file changed, +1 -1"));
        assert!(summary.contains("new.txt"));
    }

    #[test]
    fn prose_is_not_a_diff() {
        assert!(diff_summary("just some output\nwith lines\n").is_none());
        // A `+++` line alone, with no hunk or git header, is not a diff.
        assert!(diff_summary("+++ not really a diff\nsome text\n").is_none());
    }

    #[test]
    fn truncated_diff_leads_with_the_file_list() {
        let mut store = store(60); // ~180 chars; the diff below is far bigger
        let padding: String = (1..=80).map(|i| format!(" context line {i}\n")).collect();
        let gated = store.gate("shell", format!("{GIT_DIFF}{padding}"));

        assert!(gated.starts_with("[truncated diff — 3 files changed"));
        assert!(gated.contains("src/two.rs"));
        assert!(gated.contains("full output saved to"));
    }

    #[test]
    fn untruncated_output_gets_no_preamble() {
        let mut store = store(10_000);
        let gated = store.gate("shell", GIT_DIFF.to_string());
        assert_eq!(gated, GIT_DIFF);
    }
}
