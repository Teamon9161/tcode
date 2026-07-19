use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{Searcher, SearcherBuilder, Sink, SinkContext, SinkFinish, SinkMatch};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use tcode_core::{AutoSafety, BatchPolicy, PermissionRequest, Tool, ToolCtx, ToolOutput};

const DEFAULT_MATCH_LIMIT: usize = 200;
/// Cap each matched line so a single giant line (minified JS, JSONL session
/// transcripts, data blobs) cannot flood the context. head_limit bounds the
/// match *count*; this bounds the *bytes* per match.
const MAX_LINE_BYTES: usize = 512;
/// grep never reads files larger than this — content search over multi-MB
/// files is both slow and useless. This still admits ordinary source files
/// when a narrow glob selects them. Applies to grep only, not glob (name
/// search must still find large files).
const MAX_FILE_BYTES: u64 = 512 * 1024;
/// Ceiling on -A/-B/-C context so a wide window over many matches cannot
/// balloon the (un-gated) grep output.
const MAX_CONTEXT: u64 = 30;
/// Wall-clock ceiling for a single search. Cancellation (Esc) still works;
/// this is the automatic backstop so a walk over a huge tree returns a
/// clearly-marked partial result instead of hanging.
const SEARCH_DEADLINE: Duration = Duration::from_secs(10);

/// Directories we never descend into, regardless of .gitignore. This is the
/// safety net for searches pointed *outside* a git repo (e.g. the home dir),
/// where gitignore pruning does not apply and the walk would otherwise dive
/// into VCS metadata and caches with hundreds of thousands of files.
const PRUNE_DIRS: &[&str] = &[
    // version control
    ".git",
    ".svn",
    ".hg",
    ".bzr",
    ".jj",
    ".sl", //
    // build outputs
    "node_modules",
    "target",
    "dist",
    "build",
    "zig-cache",
    "zig-out",
    ".zig-cache", //
    // language / tool caches
    ".venv",
    "venv",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".tox",
    ".nox",
    ".cargo",
    ".rustup",
    ".cache",
    ".npm",
    ".pnpm-store",
    ".yarn",
    ".gradle",
    ".m2",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".turbo",
    ".parcel-cache",
    // OS
    "AppData",
];

fn walk_builder(base: &Path) -> ignore::WalkBuilder {
    let mut b = ignore::WalkBuilder::new(base);
    // Search hidden files (.github/, .config/, dotfiles are routinely wanted);
    // heavy/VCS dirs are pruned explicitly below instead of by the blunt
    // "skip everything starting with a dot" rule.
    b.hidden(false).filter_entry(|entry| {
        !(entry.file_type().is_some_and(|t| t.is_dir())
            && entry
                .file_name()
                .to_str()
                .is_some_and(|n| PRUNE_DIRS.contains(&n)))
    });
    b
}

fn rel_display(path: &Path, base: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// Trim trailing whitespace, redact credential-shaped values, and cap the line
/// at a byte budget on a char boundary, so a single enormous line can't blow
/// up the tool result.
///
/// Redaction comes before capping — the other order can cut a placeholder in
/// half and leave the tail of a real key visible. `grep` shares this with
/// `read` because it is equally never-asking, and `edit`'s contract counts
/// grep output as "seen". It only sees one line at a time, so a PEM key body
/// matched by grep is not recognized as such; that gap is acceptable since
/// this was never a boundary shell couldn't walk around anyway.
fn cap_line(line: &str) -> String {
    let s = line.trim_end();
    let redacted = crate::redact::redact_line(s);
    let s = redacted.as_deref().unwrap_or(s);
    if s.len() <= MAX_LINE_BYTES {
        return s.to_string();
    }
    let mut end = MAX_LINE_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[+{} bytes]", &s[..end], s.len() - end)
}

/// One output line: a match or a surrounding context line.
struct Line {
    lnum: u64,
    text: String,
    is_match: bool,
}

/// A contiguous block of lines (matches plus any merged context), as
/// delimited by the searcher's context breaks. Paging counts `matches`, not
/// lines, so context never distorts head_limit/offset.
struct Group {
    file: String,
    first: u64,
    matches: usize,
    lines: Vec<Line>,
}

/// Collects all matches and their context from one file. The parallel walk
/// appends completed groups to shared storage; global sorting and pagination
/// happen only after the walk has finished or reached its deadline.
struct GroupSink<'a> {
    file: String,
    groups: &'a Mutex<Vec<Group>>,
    cur: Vec<Line>,
    cur_matches: usize,
    out: Vec<Group>,
}

impl GroupSink<'_> {
    fn flush(&mut self) {
        if self.cur.is_empty() {
            return;
        }
        let lines = std::mem::take(&mut self.cur);
        self.out.push(Group {
            file: self.file.clone(),
            first: lines[0].lnum,
            matches: std::mem::take(&mut self.cur_matches),
            lines,
        });
    }
}

impl Sink for GroupSink<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _s: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        let first = mat.line_number().unwrap_or(0);
        for (offset, line) in mat.lines().enumerate() {
            self.cur.push(Line {
                lnum: first + offset as u64,
                text: cap_line(&String::from_utf8_lossy(line)),
                is_match: true,
            });
            self.cur_matches += 1;
        }
        Ok(true)
    }

    fn context(&mut self, _s: &Searcher, c: &SinkContext<'_>) -> Result<bool, Self::Error> {
        self.cur.push(Line {
            lnum: c.line_number().unwrap_or(0),
            text: cap_line(&String::from_utf8_lossy(c.bytes())),
            is_match: false,
        });
        Ok(true)
    }

    fn context_break(&mut self, _s: &Searcher) -> Result<bool, Self::Error> {
        self.flush();
        Ok(true)
    }

    fn finish(&mut self, _s: &Searcher, _: &SinkFinish) -> Result<(), Self::Error> {
        self.flush();
        if !self.out.is_empty() {
            self.groups.lock().unwrap().append(&mut self.out);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- grep

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn batch_policy(&self) -> BatchPolicy {
        BatchPolicy::ParallelReadOnly
    }

    fn display_name(&self) -> String {
        "Search".to_string()
    }

    fn batch_label(&self, inputs: &[&Value]) -> String {
        let count = inputs.len();
        format!(
            "Search {count} {}",
            if count == 1 { "pattern" } else { "patterns" }
        )
    }

    // Precise file:line list, self-capped by head_limit and per-line bytes —
    // never blob-gate.
    fn gates_output(&self) -> bool {
        false
    }

    fn description(&self) -> &str {
        "Search file contents with a regex (ripgrep engine, respects \
         .gitignore). Pull surrounding code with `context` (-C, both sides), \
         `before` (-B) or `after` (-A) — one search with context often gives \
         you enough to edit without a follow-up read. Filter files with \
         `glob`; cap matches with head_limit (default 200) and page with \
         offset. Skips files over 512 KiB and built/cache directories; reports \
         oversized skips when they explain an empty result."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regex to search for" },
                "path": { "type": "string", "description": "Directory or file to search (default: cwd)" },
                "glob": { "type": "string", "description": "Filter files, e.g. *.rs or src/**/*.toml" },
                "case_insensitive": { "type": "boolean" },
                "context": { "type": "integer", "description": "Context lines on both sides of each match (-C)" },
                "before": { "type": "integer", "description": "Context lines before each match (-B); overrides context" },
                "after": { "type": "integer", "description": "Context lines after each match (-A); overrides context" },
                "head_limit": { "type": "integer" },
                "offset": { "type": "integer", "description": "Skip this many matches before head_limit (for paging)" }
            },
            "required": ["pattern"]
        })
    }

    fn permission(&self, _input: &Value) -> PermissionRequest {
        PermissionRequest::None
    }

    fn auto_safety(&self, _input: &Value) -> AutoSafety {
        AutoSafety::Allow
    }

    fn context_paths(&self, input: &Value) -> Vec<String> {
        vec![input["path"].as_str().unwrap_or(".").to_string()]
    }

    async fn run(&self, input: Value, ctx: &ToolCtx, cancel: &CancellationToken) -> ToolOutput {
        let Some(pattern) = input["pattern"].as_str() else {
            return ToolOutput::err("missing required parameter: pattern");
        };
        let base = input["path"]
            .as_str()
            .map(|p| ctx.resolve(p))
            .unwrap_or_else(|| ctx.cwd.clone());
        if !base.exists() {
            return ToolOutput::err(format!("search path does not exist: {}", base.display()));
        }
        let matcher = match RegexMatcherBuilder::new()
            .case_insensitive(input["case_insensitive"].as_bool().unwrap_or(false))
            .build(pattern)
        {
            Ok(m) => m,
            Err(e) => {
                return ToolOutput::err(format!(
                    "invalid regex: {e}\nRemember this is regex syntax — escape literal ( ) [ ] {{ }} . * + ? with a backslash."
                ));
            }
        };
        let glob = match input["glob"].as_str() {
            Some(g) => match build_glob(g) {
                Ok(m) => Some(m),
                Err(e) => return ToolOutput::err(format!("invalid glob '{g}': {e}")),
            },
            None => None,
        };
        let limit = input["head_limit"]
            .as_u64()
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_MATCH_LIMIT)
            .max(1);
        let offset = input["offset"].as_u64().unwrap_or(0) as usize;
        // -C sets both sides; -A/-B override it. Capped so context can't blow
        // up the output.
        let ctx_c = input["context"].as_u64().unwrap_or(0);
        let before = input["before"].as_u64().unwrap_or(ctx_c).min(MAX_CONTEXT) as usize;
        let after = input["after"].as_u64().unwrap_or(ctx_c).min(MAX_CONTEXT) as usize;

        let glob_note = input["glob"]
            .as_str()
            .map(|g| format!(", glob {g}"))
            .unwrap_or_default();
        let pattern = pattern.to_string();
        let cwd = ctx.cwd.clone();
        let cancel = cancel.clone();

        // The walk reads many files and runs its own thread pool — keep it off
        // the async runtime.
        let out = tokio::task::spawn_blocking(move || {
            let groups: Mutex<Vec<Group>> = Mutex::new(Vec::new());
            let files = AtomicUsize::new(0);
            let skipped_oversized = AtomicUsize::new(0);
            let timed_out = AtomicBool::new(false);
            let start = Instant::now();

            walk_builder(&base).build_parallel().run(|| {
                let mut searcher = SearcherBuilder::new()
                    .line_number(true)
                    .before_context(before)
                    .after_context(after)
                    .build();
                let matcher = &matcher;
                let glob = glob.as_ref();
                let base: &Path = &base;
                let cwd: &Path = &cwd;
                let groups = &groups;
                let files = &files;
                let skipped_oversized = &skipped_oversized;
                let timed_out = &timed_out;
                let cancel = &cancel;
                Box::new(move |result| {
                    use ignore::WalkState;
                    if cancel.is_cancelled() {
                        return WalkState::Quit;
                    }
                    if start.elapsed() > SEARCH_DEADLINE {
                        timed_out.store(true, Ordering::Relaxed);
                        return WalkState::Quit;
                    }
                    let Ok(entry) = result else {
                        return WalkState::Continue;
                    };
                    if !entry.file_type().is_some_and(|t| t.is_file()) {
                        return WalkState::Continue;
                    }
                    let path = entry.path();
                    if let Some(g) = glob {
                        if !glob_matches(g, path, base) {
                            return WalkState::Continue;
                        }
                    }
                    if entry
                        .metadata()
                        .is_ok_and(|metadata| metadata.len() > MAX_FILE_BYTES)
                    {
                        skipped_oversized.fetch_add(1, Ordering::Relaxed);
                        return WalkState::Continue;
                    }
                    files.fetch_add(1, Ordering::Relaxed);
                    let mut sink = GroupSink {
                        file: rel_display(path, cwd),
                        groups,
                        cur: Vec::new(),
                        cur_matches: 0,
                        out: Vec::new(),
                    };
                    let _ = searcher.search_path(matcher, path, &mut sink);
                    WalkState::Continue
                })
            });

            let mut groups = groups.into_inner().unwrap();
            // Parallel walk yields files out of order; sort for stable output.
            groups.sort_by(|a, b| a.file.cmp(&b.file).then(a.first.cmp(&b.first)));
            let total: usize = groups.iter().map(|g| g.matches).sum();
            let files = files.load(Ordering::Relaxed);
            let skipped_oversized = skipped_oversized.load(Ordering::Relaxed);
            let timed_out = timed_out.load(Ordering::Relaxed);

            // Page by *matches*: keep whole groups that overlap the window so
            // context blocks stay intact.
            let mut seen = 0usize;
            let mut selected: Vec<Group> = Vec::new();
            for g in groups {
                let start = seen;
                seen += g.matches;
                if start >= offset + limit {
                    break;
                }
                if seen > offset {
                    selected.push(g);
                }
            }
            let shown: usize = selected.iter().map(|g| g.matches).sum();

            if selected.is_empty() {
                let mut m =
                    format!("no matches for /{pattern}/ ({files} files scanned{glob_note})");
                if skipped_oversized > 0 {
                    let noun = if skipped_oversized == 1 { "file" } else { "files" };
                    m.push_str(&format!(
                        "\n[{skipped_oversized} {noun} over {} KiB skipped — narrow `path` and use `read`, or use shell for an unbounded search]",
                        MAX_FILE_BYTES / 1024
                    ));
                }
                m.push_str("\n[.gitignore entries and built/cache directories are excluded]");
                if timed_out {
                    m.push_str(&format!(
                        "\n[search timed out after {}s before finishing — narrow the path or glob]",
                        SEARCH_DEADLINE.as_secs()
                    ));
                }
                return m;
            }
            let joiner = if before > 0 || after > 0 {
                "\n--\n"
            } else {
                "\n"
            };
            let mut out = selected
                .iter()
                .map(|g| {
                    g.lines
                        .iter()
                        .map(|l| {
                            let sep = if l.is_match { ':' } else { '-' };
                            format!("{}:{}{sep} {}", g.file, l.lnum, l.text)
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .collect::<Vec<_>>()
                .join(joiner);
            if timed_out {
                out.push_str(&format!(
                    "\n[search timed out after {}s — partial results; narrow the path or glob]",
                    SEARCH_DEADLINE.as_secs()
                ));
            } else if total > offset + shown {
                out.push_str(&format!(
                    "\n[more matches beyond this page — raise head_limit or set offset={}]",
                    offset + shown
                ));
            }
            out
        })
        .await;

        match out {
            Ok(s) => ToolOutput::ok(s),
            Err(e) => ToolOutput::err(format!("grep task failed: {e}")),
        }
    }
}

// ---------------------------------------------------------------- glob

pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn batch_policy(&self) -> BatchPolicy {
        BatchPolicy::ParallelReadOnly
    }

    fn display_name(&self) -> String {
        "Find".to_string()
    }

    fn batch_label(&self, inputs: &[&Value]) -> String {
        let count = inputs.len();
        format!(
            "Find {count} {}",
            if count == 1 { "pattern" } else { "patterns" }
        )
    }

    // Precise path list, capped at 200 paths — never blob-gate.
    fn gates_output(&self) -> bool {
        false
    }

    fn description(&self) -> &str {
        "Find files by name pattern, e.g. **/*.rs or src/**/Cargo.toml. \
         Respects .gitignore. Results sorted by modification time (newest \
         first), capped at 200; page with offset."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string" },
                "path": { "type": "string", "description": "Base directory (default: cwd)" },
                "offset": { "type": "integer", "description": "Skip this many results before the 200 cap (for paging)" }
            },
            "required": ["pattern"]
        })
    }

    fn permission(&self, _input: &Value) -> PermissionRequest {
        PermissionRequest::None
    }

    fn auto_safety(&self, _input: &Value) -> AutoSafety {
        AutoSafety::Allow
    }

    fn context_paths(&self, input: &Value) -> Vec<String> {
        vec![input["path"].as_str().unwrap_or(".").to_string()]
    }

    async fn run(&self, input: Value, ctx: &ToolCtx, cancel: &CancellationToken) -> ToolOutput {
        let Some(pattern) = input["pattern"].as_str() else {
            return ToolOutput::err("missing required parameter: pattern");
        };
        let base = input["path"]
            .as_str()
            .map(|p| ctx.resolve(p))
            .unwrap_or_else(|| ctx.cwd.clone());
        let glob = match build_glob(pattern) {
            Ok(g) => g,
            Err(e) => return ToolOutput::err(format!("invalid glob '{pattern}': {e}")),
        };
        let offset = input["offset"].as_u64().unwrap_or(0) as usize;
        let mut hits: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
        let mut timed_out = false;
        let start = Instant::now();
        for entry in walk_builder(&base).build() {
            if cancel.is_cancelled() {
                break;
            }
            if start.elapsed() > SEARCH_DEADLINE {
                timed_out = true;
                break;
            }
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            if path.is_file() && glob_matches(&glob, path, &base) {
                let mtime = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                hits.push((mtime, path.to_path_buf()));
            }
        }
        if hits.is_empty() {
            let mut m = format!(
                "no files match {pattern} under {}",
                rel_display(&base, &ctx.cwd)
            );
            if timed_out {
                m.push_str(&format!(
                    "\n[search timed out after {}s before finishing]",
                    SEARCH_DEADLINE.as_secs()
                ));
            }
            return ToolOutput::ok(m);
        }
        hits.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime));
        let total = hits.len();
        let page: Vec<String> = hits
            .into_iter()
            .skip(offset)
            .take(200)
            .map(|(_, p)| rel_display(&p, &ctx.cwd))
            .collect();
        let shown = page.len();
        let mut out = page.join("\n");
        if timed_out {
            out.push_str(&format!(
                "\n[search timed out after {}s — partial results]",
                SEARCH_DEADLINE.as_secs()
            ));
        } else if total > offset + shown {
            out.push_str(&format!(
                "\n[{total} matches; showing {}-{} — set offset={} for more]",
                offset + 1,
                offset + shown,
                offset + shown
            ));
        }
        ToolOutput::ok(out)
    }
}

fn build_glob(pattern: &str) -> Result<globset::GlobMatcher, globset::Error> {
    Ok(globset::GlobBuilder::new(pattern)
        .literal_separator(false)
        .build()?
        .compile_matcher())
}

/// Match against slash-normalized paths relative to the search base so
/// `src/**/*.rs` and bare names such as `app.rs` work on every platform.
fn glob_matches(glob: &globset::GlobMatcher, path: &Path, base: &Path) -> bool {
    let rel = path.strip_prefix(base).unwrap_or(path);
    let rel = rel.to_string_lossy().replace('\\', "/");
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().replace('\\', "/"));
    glob.is_match(&rel) || name.is_some_and(|name| glob.is_match(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tcode_core::ToolCtx;

    async fn grep(dir: &Path, input: Value) -> String {
        let ctx = ToolCtx::new(dir.to_path_buf(), 100_000);
        GrepTool
            .run(input, &ctx, &CancellationToken::new())
            .await
            .content
    }

    async fn glob(dir: &Path, pattern: &str) -> String {
        let ctx = ToolCtx::new(dir.to_path_buf(), 100_000);
        GlobTool
            .run(
                json!({ "pattern": pattern }),
                &ctx,
                &CancellationToken::new(),
            )
            .await
            .content
    }

    fn scratch(name: &str, body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tcode-grep-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), body).unwrap();
        dir
    }

    const BODY: &str = "line1\nline2\nTARGET\nline4\nline5\n";

    #[tokio::test]
    async fn no_context_returns_bare_match() {
        let dir = scratch("bare", BODY);
        let out = grep(&dir, json!({ "pattern": "TARGET" })).await;
        assert_eq!(out, "a.rs:3: TARGET");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn context_wraps_match_with_dash_lines() {
        let dir = scratch("ctx", BODY);
        let out = grep(&dir, json!({ "pattern": "TARGET", "context": 1 })).await;
        assert_eq!(out, "a.rs:2- line2\na.rs:3: TARGET\na.rs:4- line4");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn before_and_after_are_independent() {
        let dir = scratch("ba", BODY);
        let before = grep(
            &dir,
            json!({ "pattern": "TARGET", "before": 1, "after": 0 }),
        )
        .await;
        assert_eq!(before, "a.rs:2- line2\na.rs:3: TARGET");
        let after = grep(
            &dir,
            json!({ "pattern": "TARGET", "after": 2, "before": 0 }),
        )
        .await;
        assert_eq!(after, "a.rs:3: TARGET\na.rs:4- line4\na.rs:5- line5");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn grep_glob_matches_a_bare_filename_under_a_limited_directory() {
        let dir = scratch("bare-glob", "outside\n");
        let source = dir.join("src");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("app.rs"), "TARGET\n").unwrap();

        let out = grep(
            &dir,
            json!({ "pattern": "TARGET", "path": "src", "glob": "app.rs" }),
        )
        .await;
        assert!(out.ends_with("app.rs:1: TARGET"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn grep_glob_searches_source_files_over_the_legacy_size_limit() {
        let dir = scratch("large-glob", "");
        let content = format!("{}\nTARGET\n", "x".repeat(256 * 1024));
        std::fs::write(dir.join("large.rs"), content).unwrap();

        let out = grep(&dir, json!({ "pattern": "TARGET", "glob": "large.rs" })).await;
        assert!(out.ends_with("large.rs:2: TARGET"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn grep_reports_files_skipped_by_its_size_limit() {
        let dir = scratch("oversized", "");
        let content = format!("{}\nTARGET\n", "x".repeat(MAX_FILE_BYTES as usize));
        std::fs::write(dir.join("large.rs"), content).unwrap();

        let out = grep(&dir, json!({ "pattern": "TARGET", "glob": "large.rs" })).await;
        assert!(out.starts_with("no matches for /TARGET/ (0 files scanned, glob large.rs)"));
        assert!(out.contains("[1 file over 512 KiB skipped"), "{out}");
        assert!(
            out.contains("[.gitignore entries and built/cache directories are excluded]"),
            "{out}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cache_directories_are_pruned_for_grep_and_glob() {
        let dir = scratch("cache-prune", "TARGET\n");
        for cache in ["zig-cache", "zig-out", ".pytest_cache", ".next"] {
            let cache_dir = dir.join(cache);
            std::fs::create_dir_all(&cache_dir).unwrap();
            std::fs::write(cache_dir.join("hidden.rs"), "TARGET\n").unwrap();
        }

        let grep_out = grep(&dir, json!({ "pattern": "TARGET" })).await;
        assert!(grep_out.contains("a.rs:1: TARGET"), "{grep_out}");
        for cache in ["zig-cache", "zig-out", ".pytest_cache", ".next"] {
            assert!(!grep_out.contains(cache), "{grep_out}");
        }

        let glob_out = glob(&dir, "*.rs").await;
        assert!(glob_out.contains("a.rs"), "{glob_out}");
        for cache in ["zig-cache", "zig-out", ".pytest_cache", ".next"] {
            assert!(!glob_out.contains(cache), "{glob_out}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn grep_paging_uses_global_path_order() {
        let dir = scratch("paging", "TARGET a1\n");
        std::fs::write(dir.join("b.rs"), "TARGET b1\n").unwrap();
        std::fs::write(dir.join("z.rs"), "TARGET z1\n").unwrap();

        let first = grep(&dir, json!({ "pattern": "TARGET", "head_limit": 1 })).await;
        let second = grep(
            &dir,
            json!({ "pattern": "TARGET", "head_limit": 1, "offset": 1 }),
        )
        .await;
        let third = grep(
            &dir,
            json!({ "pattern": "TARGET", "head_limit": 1, "offset": 2 }),
        )
        .await;

        assert!(first.starts_with("a.rs:1: TARGET a1"), "{first}");
        assert!(second.starts_with("b.rs:1: TARGET b1"), "{second}");
        assert!(third.starts_with("z.rs:1: TARGET z1"), "{third}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn separate_matches_break_with_marker() {
        let dir = scratch("break", "hit\nx\nx\nx\nx\nx\nhit\n");
        let out = grep(&dir, json!({ "pattern": "hit", "context": 1 })).await;
        // Two matches far apart: distinct blocks joined by `--`.
        assert_eq!(out, "a.rs:1: hit\na.rs:2- x\n--\na.rs:6- x\na.rs:7: hit");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
