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
/// Per-file ceiling on matches. head_limit alone is not enough: files are
/// emitted in path order, so one file with hundreds of hits (a generated
/// binding, a lockfile-ish table, a test with a repeated symbol) eats the
/// whole budget and the model never learns the pattern also occurs in ten
/// other places. Capping per file trades depth in one file — reachable with
/// `path` — for breadth across the tree, which is what a search is for.
const MAX_MATCHES_PER_FILE: usize = 30;
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

fn walk_builder(base: &Path, follow_links: bool) -> ignore::WalkBuilder {
    let mut b = ignore::WalkBuilder::new(base);
    // Search hidden files (.github/, .config/, dotfiles are routinely wanted);
    // heavy/VCS dirs are pruned explicitly below instead of by the blunt
    // "skip everything starting with a dot" rule.
    b.follow_links(follow_links)
        .hidden(false)
        .filter_entry(|entry| {
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

/// Cut a group down to `take` matches starting at its `skip`-th, so a group
/// that straddles a page edge contributes only the matches inside the window.
///
/// The kept matches keep their context: the cut runs from `before` lines ahead
/// of the first one to `after` lines past the last, which is exactly the block
/// those matches would have produced had they been the only ones in the file.
/// Nothing dangles at either end.
fn clip_to_window(g: &mut Group, skip: usize, take: usize, before: usize, after: usize) {
    if skip == 0 && take >= g.matches {
        return;
    }
    let match_at: Vec<usize> = g
        .lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.is_match)
        .map(|(i, _)| i)
        .collect();
    let Some(&first) = match_at.get(skip) else {
        g.lines.clear();
        g.matches = 0;
        return;
    };
    let last = match_at[(skip + take - 1).min(match_at.len() - 1)];
    let lo = first.saturating_sub(before);
    let hi = (last + after).min(g.lines.len() - 1);
    g.lines.drain(hi + 1..);
    g.lines.drain(..lo);
    g.first = g.lines[0].lnum;
    g.matches = take.min(match_at.len() - skip);
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
         .gitignore). Look for several symbols in one call with alternation — \
         `foo|bar|baz` beats three searches. An all-lowercase pattern matches \
         case-insensitively; any uppercase in it makes the match exact. Pull \
         surrounding code with `context` (-C, both sides), `before` (-B) or \
         `after` (-A) — one search with context often gives you enough to \
         edit without a follow-up read. Filter files with `glob`; cap matches \
         with head_limit (default 200) and page with offset. Skips files over \
         512 KiB and built/cache directories; reports oversized skips when \
         they explain an empty result."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regex to search for" },
                "path": { "type": "string", "description": "Directory or file to search (default: cwd)" },
                "glob": { "type": "string", "description": "Filter files, e.g. *.rs or src/**/*.toml" },
                "case_insensitive": { "type": "boolean", "description": "Force case-insensitive; only needed to widen a pattern that contains uppercase" },
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
        // Smart case: an all-lowercase pattern searches case-insensitively, an
        // uppercase-bearing one stays exact. `case_insensitive` still wins
        // outright (it short-circuits in grep-regex), so explicit intent is
        // never overridden — this only fills in the case the model would
        // otherwise discover by searching twice.
        let matcher = match RegexMatcherBuilder::new()
            .case_insensitive(input["case_insensitive"].as_bool().unwrap_or(false))
            .case_smart(true)
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

            walk_builder(&base, false).build_parallel().run(|| {
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

            // Apply the per-file cap before paging, so head_limit/offset count
            // the matches actually reachable through this tool and paging stays
            // self-consistent. The cut is at match granularity, not group
            // granularity: with no context lines an entire file arrives as one
            // group, so dropping whole groups would never fire. Trailing
            // after-context past the last kept match goes too, leaving no
            // dangling context. Single-file results are exempt — nothing can be
            // crowded out, and a search aimed at one file should page with
            // offset rather than silently lose its tail.
            let mut hidden = 0usize;
            let mut capped_files = 0usize;
            if groups.windows(2).any(|w| w[0].file != w[1].file) {
                let mut kept = Vec::with_capacity(groups.len());
                let mut file = String::new();
                let mut in_file = 0usize;
                for mut g in groups {
                    if g.file != file {
                        file.clone_from(&g.file);
                        in_file = 0;
                    }
                    let allowance = MAX_MATCHES_PER_FILE.saturating_sub(in_file);
                    if allowance == 0 {
                        hidden += g.matches;
                        continue;
                    }
                    if g.matches <= allowance {
                        in_file += g.matches;
                        kept.push(g);
                        continue;
                    }
                    // First group to cross the cap for this file: keep matches
                    // up to the allowance, then cut.
                    capped_files += 1;
                    let mut taken = 0usize;
                    let mut cut = g.lines.len();
                    for (i, line) in g.lines.iter().enumerate() {
                        if line.is_match {
                            taken += 1;
                            if taken == allowance {
                                cut = i + 1;
                                break;
                            }
                        }
                    }
                    hidden += g.matches - taken;
                    g.lines.truncate(cut);
                    g.matches = taken;
                    in_file = MAX_MATCHES_PER_FILE;
                    kept.push(g);
                }
                groups = kept;
            }

            let total: usize = groups.iter().map(|g| g.matches).sum();
            let files = files.load(Ordering::Relaxed);
            let skipped_oversized = skipped_oversized.load(Ordering::Relaxed);
            let timed_out = timed_out.load(Ordering::Relaxed);

            // Page by *matches*, cutting the groups that straddle a window edge
            // instead of keeping them whole. Keeping them whole is what the
            // per-file cap above already learned not to do: with no context
            // lines a file's every match merges into one group, so a group is
            // usually the whole file and "keep whole groups" degenerates into
            // "ignore head_limit". `grep pattern head_limit=5` over a file with
            // 40 hits returned all of them, and then advertised `offset=30` as
            // the next page — a limit the model cannot use and a cursor that
            // does not match what it was given.
            let window_end = offset.saturating_add(limit);
            let mut seen = 0usize;
            let mut selected: Vec<Group> = Vec::new();
            for mut g in groups {
                let start = seen;
                seen += g.matches;
                if start >= window_end {
                    break;
                }
                if seen <= offset {
                    continue;
                }
                if start < offset || seen > window_end {
                    clip_to_window(
                        &mut g,
                        offset.saturating_sub(start),
                        seen.min(window_end) - offset.max(start),
                        before,
                        after,
                    );
                }
                selected.push(g);
            }
            let shown: usize = selected.iter().map(|g| g.matches).sum();

            if selected.is_empty() && total > 0 {
                // Matches exist, the requested page is past the end. Saying "no
                // matches" here would send the model hunting for a bad pattern.
                return format!(
                    "offset={offset} is past the last of {total} matches for /{pattern}/{glob_note} — lower offset or drop it"
                );
            }
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
            // A path is needed to locate a hit, but repeating it on every
            // line is costly for files with many matches or context lines.
            // Groups are sorted by file and line, so emit one heading per file
            // and retain the `--` separator only between disjoint context
            // blocks in that same file.
            let with_context = before > 0 || after > 0;
            let mut out = String::new();
            let mut previous_file: Option<&str> = None;
            for g in &selected {
                let same_file = previous_file == Some(g.file.as_str());
                if !out.is_empty() {
                    if same_file && with_context {
                        out.push_str("\n--\n");
                    } else {
                        out.push('\n');
                    }
                }
                if !same_file {
                    out.push_str(&g.file);
                    out.push_str(":\n");
                }
                for (index, line) in g.lines.iter().enumerate() {
                    if index > 0 {
                        out.push('\n');
                    }
                    let sep = if line.is_match { ':' } else { '-' };
                    out.push_str(&format!("{}{sep} {}", line.lnum, line.text));
                }
                previous_file = Some(&g.file);
            }
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
            if hidden > 0 {
                let noun = if capped_files == 1 { "file" } else { "files" };
                out.push_str(&format!(
                    "\n[{hidden} further matches in {capped_files} {noun} not shown — over {MAX_MATCHES_PER_FILE} per file; re-run with `path` set to one of them for the rest]"
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
         Respects .gitignore. Directory symlinks are skipped by default; set \
         follow_symlinks=true to search their targets. Results sorted by \
         modification time (newest first), capped at 200; page with offset."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string" },
                "path": { "type": "string", "description": "Base directory (default: cwd)" },
                "follow_symlinks": { "type": "boolean", "description": "Follow directory symlinks while walking (default false)" },
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
        let follow_links = input["follow_symlinks"].as_bool().unwrap_or(false);

        // Same shape as grep: a parallel walk, off the async runtime. The walk
        // is blocking syscalls end to end, so leaving it in the async body both
        // pinned a runtime thread and serialized any batch it was part of.
        let walk_base = base.clone();
        let walk_cancel = cancel.clone();
        let (mut hits, timed_out, skipped_directory_links) =
            match tokio::task::spawn_blocking(move || {
                let hits: Mutex<Vec<(std::time::SystemTime, PathBuf)>> = Mutex::new(Vec::new());
                let timed_out = AtomicBool::new(false);
                let skipped_directory_links = AtomicUsize::new(0);
                let start = Instant::now();
                walk_builder(&walk_base, follow_links)
                    .build_parallel()
                    .run(|| {
                        let glob = &glob;
                        let base: &Path = &walk_base;
                        let hits = &hits;
                        let timed_out = &timed_out;
                        let skipped_directory_links = &skipped_directory_links;
                        let cancel = &walk_cancel;
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
                            if !follow_links
                                && entry.path_is_symlink()
                                // The walker exposes symlink metadata while it is not
                                // following links. Query the path to test the link target.
                                && std::fs::metadata(entry.path())
                                    .is_ok_and(|metadata| metadata.is_dir())
                            {
                                skipped_directory_links.fetch_add(1, Ordering::Relaxed);
                                return WalkState::Continue;
                            }
                            // The walk already knows the file type — `path.is_file()`
                            // spent an extra stat on every entry in the tree.
                            if !entry.file_type().is_some_and(|t| t.is_file()) {
                                return WalkState::Continue;
                            }
                            let path = entry.path();
                            if !glob_matches(glob, path, base) {
                                return WalkState::Continue;
                            }
                            let mtime = entry
                                .metadata()
                                .ok()
                                .and_then(|m| m.modified().ok())
                                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                            // Locked only on a hit, not per entry walked.
                            hits.lock().unwrap().push((mtime, path.to_path_buf()));
                            WalkState::Continue
                        })
                    });
                (
                    hits.into_inner().unwrap(),
                    timed_out.load(Ordering::Relaxed),
                    skipped_directory_links.load(Ordering::Relaxed),
                )
            })
            .await
            {
                Ok(out) => out,
                Err(e) => return ToolOutput::err(format!("glob task failed: {e}")),
            };

        if hits.is_empty() {
            let mut m = format!(
                "no files match {pattern} under {}",
                rel_display(&base, &ctx.cwd)
            );
            if skipped_directory_links > 0 {
                let noun = if skipped_directory_links == 1 {
                    "directory symlink was"
                } else {
                    "directory symlinks were"
                };
                m.push_str(&format!(
                    "\n[{skipped_directory_links} {noun} skipped — set follow_symlinks=true to search their targets]"
                ));
            }
            if timed_out {
                m.push_str(&format!(
                    "\n[search timed out after {}s before finishing]",
                    SEARCH_DEADLINE.as_secs()
                ));
            }
            return ToolOutput::ok(m);
        }
        // Newest first, ties broken by path. The tie-break is load-bearing now
        // that the walk is parallel: mtime alone left same-second files in
        // thread-completion order, so two identical calls could disagree and
        // `offset` paging would skip or repeat entries.
        hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
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
        let ctx = ToolCtx::for_test(dir.to_path_buf(), 100_000);
        GrepTool
            .run(input, &ctx, &CancellationToken::new())
            .await
            .content
    }

    async fn glob_input(dir: &Path, input: Value) -> String {
        let ctx = ToolCtx::for_test(dir.to_path_buf(), 100_000);
        GlobTool
            .run(input, &ctx, &CancellationToken::new())
            .await
            .content
    }

    async fn glob(dir: &Path, pattern: &str) -> String {
        glob_input(dir, json!({ "pattern": pattern })).await
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
        assert_eq!(out, "a.rs:\n3: TARGET");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn context_wraps_match_with_dash_lines() {
        let dir = scratch("ctx", BODY);
        let out = grep(&dir, json!({ "pattern": "TARGET", "context": 1 })).await;
        assert_eq!(out, "a.rs:\n2- line2\n3: TARGET\n4- line4");
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
        assert_eq!(before, "a.rs:\n2- line2\n3: TARGET");
        let after = grep(
            &dir,
            json!({ "pattern": "TARGET", "after": 2, "before": 0 }),
        )
        .await;
        assert_eq!(after, "a.rs:\n3: TARGET\n4- line4\n5- line5");
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
        assert!(out.ends_with("app.rs:\n1: TARGET"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn grep_glob_searches_source_files_over_the_legacy_size_limit() {
        let dir = scratch("large-glob", "");
        let content = format!("{}\nTARGET\n", "x".repeat(256 * 1024));
        std::fs::write(dir.join("large.rs"), content).unwrap();

        let out = grep(&dir, json!({ "pattern": "TARGET", "glob": "large.rs" })).await;
        assert!(out.ends_with("large.rs:\n2: TARGET"), "{out}");
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
        assert!(grep_out.contains("a.rs:\n1: TARGET"), "{grep_out}");
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

        assert!(first.starts_with("a.rs:\n1: TARGET a1"), "{first}");
        assert!(second.starts_with("b.rs:\n1: TARGET b1"), "{second}");
        assert!(third.starts_with("z.rs:\n1: TARGET z1"), "{third}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Matches inside one file merge into a single group when no context is
    /// asked for, so paging that keeps whole groups ignores head_limit
    /// entirely — the case that used to return all 8 hits for head_limit=3.
    #[tokio::test]
    async fn head_limit_counts_matches_inside_one_file() {
        let body: String = (1..=8).map(|i| format!("TARGET {i}\n")).collect();
        let dir = scratch("within-file", &body);

        let page = grep(&dir, json!({ "pattern": "TARGET", "head_limit": 3 })).await;
        assert_eq!(
            page,
            "a.rs:\n1: TARGET 1\n2: TARGET 2\n3: TARGET 3\
             \n[more matches beyond this page — raise head_limit or set offset=3]"
        );

        let next = grep(
            &dir,
            json!({ "pattern": "TARGET", "head_limit": 3, "offset": 3 }),
        )
        .await;
        assert_eq!(
            next,
            "a.rs:\n4: TARGET 4\n5: TARGET 5\n6: TARGET 6\
             \n[more matches beyond this page — raise head_limit or set offset=6]"
        );

        let last = grep(
            &dir,
            json!({ "pattern": "TARGET", "head_limit": 3, "offset": 6 }),
        )
        .await;
        assert_eq!(last, "a.rs:\n7: TARGET 7\n8: TARGET 8");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A clipped page keeps each surviving match's context and drops the rest,
    /// so no context line dangles without the match it belongs to.
    #[tokio::test]
    async fn a_clipped_page_keeps_the_context_of_the_matches_it_kept() {
        let dir = scratch(
            "clip-ctx",
            "x1\nTARGET a\nx3\nx4\nTARGET b\nx6\nx7\nTARGET c\nx9\n",
        );

        let first = grep(
            &dir,
            json!({ "pattern": "TARGET", "context": 1, "head_limit": 1 }),
        )
        .await;
        assert_eq!(
            first,
            "a.rs:\n1- x1\n2: TARGET a\n3- x3\
             \n[more matches beyond this page — raise head_limit or set offset=1]"
        );

        let middle = grep(
            &dir,
            json!({ "pattern": "TARGET", "context": 1, "head_limit": 1, "offset": 1 }),
        )
        .await;
        assert_eq!(
            middle,
            "a.rs:\n4- x4\n5: TARGET b\n6- x6\
             \n[more matches beyond this page — raise head_limit or set offset=2]"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The window can open inside one file and close inside the next: both
    /// edges get cut, and the count in between is exactly head_limit.
    #[tokio::test]
    async fn a_page_can_straddle_two_files() {
        let body: String = (1..=4).map(|i| format!("TARGET a{i}\n")).collect();
        let dir = scratch("straddle", &body);
        let other: String = (1..=4).map(|i| format!("TARGET b{i}\n")).collect();
        std::fs::write(dir.join("b.rs"), &other).unwrap();

        let out = grep(
            &dir,
            json!({ "pattern": "TARGET", "head_limit": 4, "offset": 2 }),
        )
        .await;
        assert_eq!(
            out,
            "a.rs:\n3: TARGET a3\n4: TARGET a4\n\
             b.rs:\n1: TARGET b1\n2: TARGET b2\
             \n[more matches beyond this page — raise head_limit or set offset=6]"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::windows::fs::symlink_dir(target, link)
    }

    #[tokio::test]
    async fn glob_reports_skipped_directory_symlinks_and_can_follow_them() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let skill = temp.path().join("arbor-skill");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "skill\n").unwrap();
        let link = root.join("arbor");
        let link_result = symlink_dir(&skill, &link);
        #[cfg(windows)]
        if link_result.as_ref().is_err_and(|error| {
            error.kind() == std::io::ErrorKind::PermissionDenied
                || error.raw_os_error() == Some(1314)
        }) {
            return;
        }
        link_result.unwrap();

        let skipped = glob(&root, "**/SKILL.md").await;
        assert!(
            skipped.starts_with("no files match **/SKILL.md"),
            "{skipped}"
        );
        assert!(
            skipped.contains(
                "[1 directory symlink was skipped — set follow_symlinks=true to search their targets]"
            ),
            "{skipped}"
        );

        let followed = glob_input(
            &root,
            json!({ "pattern": "**/SKILL.md", "follow_symlinks": true }),
        )
        .await;
        assert_eq!(followed, "arbor/SKILL.md");
    }

    #[tokio::test]
    async fn glob_order_is_stable_when_mtimes_tie() {
        let dir = scratch("glob-stable", "");
        let stamp = std::time::SystemTime::now();
        let mut names: Vec<String> = (0..40).map(|i| format!("f{i:02}.rs")).collect();
        names.push("a.rs".to_string()); // written by `scratch`
        for name in &names {
            let path = dir.join(name);
            std::fs::write(&path, "x\n").unwrap();
            // Identical mtimes across all files: ordering must come from the
            // path tie-break, not from whichever walk thread finished first.
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(stamp))
                .unwrap();
        }
        names.sort();
        let expected = names.join("\n");

        for _ in 0..5 {
            assert_eq!(glob(&dir, "*.rs").await, expected);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn lowercase_pattern_is_smart_case_and_uppercase_stays_exact() {
        let dir = scratch("smartcase", "Target\nTARGET\ntarget\n");

        let lower = grep(&dir, json!({ "pattern": "target" })).await;
        assert_eq!(
            lower, "a.rs:\n1: Target\n2: TARGET\n3: target",
            "an all-lowercase pattern should match every casing"
        );

        let mixed = grep(&dir, json!({ "pattern": "Target" })).await;
        assert_eq!(
            mixed, "a.rs:\n1: Target",
            "uppercase in the pattern is exact"
        );

        // The explicit knob still wins over smart case.
        let forced = grep(
            &dir,
            json!({ "pattern": "Target", "case_insensitive": true }),
        )
        .await;
        assert_eq!(forced, "a.rs:\n1: Target\n2: TARGET\n3: target");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn one_crowded_file_cannot_hide_matches_in_other_files() {
        let dir = scratch("percap", "");
        let crowded = (0..MAX_MATCHES_PER_FILE + 5)
            .map(|i| format!("TARGET {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.join("a.rs"), crowded).unwrap();
        std::fs::write(dir.join("z.rs"), "TARGET tail\n").unwrap();

        let out = grep(&dir, json!({ "pattern": "TARGET" })).await;
        assert_eq!(
            out.lines()
                .skip(1)
                .take_while(|line| *line != "z.rs:")
                .filter(|line| line
                    .as_bytes()
                    .first()
                    .is_some_and(|byte| byte.is_ascii_digit()))
                .count(),
            MAX_MATCHES_PER_FILE
        );
        assert!(
            out.contains("z.rs:\n1: TARGET tail"),
            "the other file must survive the crowded one: {out}"
        );
        assert!(
            out.contains("[5 further matches in 1 file not shown"),
            "the cap must explain itself: {out}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_single_file_result_is_exempt_from_the_per_file_cap() {
        let dir = scratch("percap-single", "");
        let body = (0..MAX_MATCHES_PER_FILE + 5)
            .map(|i| format!("TARGET {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.join("a.rs"), body).unwrap();

        let out = grep(&dir, json!({ "pattern": "TARGET" })).await;
        assert_eq!(
            out.lines()
                .skip(1)
                .filter(|line| line
                    .as_bytes()
                    .first()
                    .is_some_and(|byte| byte.is_ascii_digit()))
                .count(),
            MAX_MATCHES_PER_FILE + 5,
            "nothing can be crowded out when there is only one file: {out}"
        );
        assert!(!out.contains("not shown"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn offset_past_the_end_says_so_instead_of_no_matches() {
        let dir = scratch("overrun", BODY);
        let out = grep(&dir, json!({ "pattern": "TARGET", "offset": 50 })).await;
        assert!(
            out.starts_with("offset=50 is past the last of 1 matches for /TARGET/"),
            "{out}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn separate_matches_break_with_marker() {
        let dir = scratch("break", "hit\nx\nx\nx\nx\nx\nhit\n");
        let out = grep(&dir, json!({ "pattern": "hit", "context": 1 })).await;
        // Two matches far apart: distinct blocks joined by `--`.
        assert_eq!(out, "a.rs:\n1: hit\n2- x\n--\n6- x\n7: hit");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
