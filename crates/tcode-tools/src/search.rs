use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::sinks::UTF8;
use grep_searcher::SearcherBuilder;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use tcode_core::{PermissionRequest, Tool, ToolCtx, ToolOutput};

const DEFAULT_MATCH_LIMIT: usize = 200;
/// Cap each matched line so a single giant line (minified JS, JSONL session
/// transcripts, data blobs) cannot flood the context. head_limit bounds the
/// match *count*; this bounds the *bytes* per match.
const MAX_LINE_BYTES: usize = 512;
/// grep never reads files larger than this — content search over multi-MB
/// files is both slow and useless. Applies to grep only, not glob (name
/// search must still find large files).
const MAX_FILE_BYTES: u64 = 256 * 1024;
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
    ".git", ".svn", ".hg", ".bzr", ".jj", ".sl", //
    // build outputs
    "node_modules", "target", "dist", "build", //
    // language / tool caches
    ".venv", "venv", "__pycache__", ".cargo", ".rustup", ".cache", ".npm", ".gradle", ".m2",
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

/// Trim trailing whitespace and cap the line at a byte budget on a char
/// boundary, so a single enormous line can't blow up the tool result.
fn cap_line(line: &str) -> String {
    let s = line.trim_end();
    if s.len() <= MAX_LINE_BYTES {
        return s.to_string();
    }
    let mut end = MAX_LINE_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[+{} bytes]", &s[..end], s.len() - end)
}

// ---------------------------------------------------------------- grep

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    // Precise file:line list, self-capped by head_limit and per-line bytes —
    // never blob-gate.
    fn gates_output(&self) -> bool {
        false
    }

    fn description(&self) -> &str {
        "Search file contents with a regex (ripgrep engine, respects \
         .gitignore). Returns matching lines as path:line:text. Filter \
         files with `glob`; cap output with head_limit (default 200) and \
         page with offset."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regex to search for" },
                "path": { "type": "string", "description": "Directory or file to search (default: cwd)" },
                "glob": { "type": "string", "description": "Filter files, e.g. *.rs or src/**/*.toml" },
                "case_insensitive": { "type": "boolean" },
                "head_limit": { "type": "integer" },
                "offset": { "type": "integer", "description": "Skip this many matches before head_limit (for paging)" }
            },
            "required": ["pattern"]
        })
    }

    fn permission(&self, _input: &Value) -> PermissionRequest {
        PermissionRequest::None
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
        // Collect just enough to fill the requested page, then stop.
        let want = offset.saturating_add(limit);

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
            let matches: Mutex<Vec<(String, u64, String)>> = Mutex::new(Vec::new());
            let count = AtomicUsize::new(0);
            let files = AtomicUsize::new(0);
            let timed_out = AtomicBool::new(false);
            let start = Instant::now();

            let mut builder = walk_builder(&base);
            builder.max_filesize(Some(MAX_FILE_BYTES));
            builder.build_parallel().run(|| {
                let mut searcher = SearcherBuilder::new().line_number(true).build();
                let matcher = &matcher;
                let glob = glob.as_ref();
                let base: &Path = &base;
                let cwd: &Path = &cwd;
                let matches = &matches;
                let count = &count;
                let files = &files;
                let timed_out = &timed_out;
                let cancel = &cancel;
                Box::new(move |result| {
                    use ignore::WalkState;
                    if cancel.is_cancelled() || count.load(Ordering::Relaxed) >= want {
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
                    files.fetch_add(1, Ordering::Relaxed);
                    let display = rel_display(path, cwd);
                    let mut local: Vec<(String, u64, String)> = Vec::new();
                    let _ = searcher.search_path(
                        matcher,
                        path,
                        UTF8(|lnum, line| {
                            local.push((display.clone(), lnum, cap_line(line)));
                            // A single file can't contribute more than the page.
                            Ok(local.len() < want)
                        }),
                    );
                    if !local.is_empty() {
                        count.fetch_add(local.len(), Ordering::Relaxed);
                        matches.lock().unwrap().extend(local);
                    }
                    WalkState::Continue
                })
            });

            let mut hits = matches.into_inner().unwrap();
            // Parallel walk yields matches out of order; sort for stable output.
            hits.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
            let total = hits.len();
            let page: Vec<_> = hits.into_iter().skip(offset).take(limit).collect();
            let files = files.load(Ordering::Relaxed);
            let timed_out = timed_out.load(Ordering::Relaxed);

            if page.is_empty() {
                let mut m =
                    format!("no matches for /{pattern}/ ({files} files scanned{glob_note})");
                if timed_out {
                    m.push_str(&format!(
                        "\n[search timed out after {}s before finishing — narrow the path or glob]",
                        SEARCH_DEADLINE.as_secs()
                    ));
                }
                return m;
            }
            let shown = page.len();
            let mut out = page
                .into_iter()
                .map(|(d, l, t)| format!("{d}:{l}: {t}"))
                .collect::<Vec<_>>()
                .join("\n");
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
        hits.sort_by(|a, b| b.0.cmp(&a.0));
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

/// Match against the path relative to the search base so `src/**/*.rs`
/// works regardless of where the base directory lives.
fn glob_matches(glob: &globset::GlobMatcher, path: &Path, base: &Path) -> bool {
    let rel = path.strip_prefix(base).unwrap_or(path);
    glob.is_match(rel) || path.file_name().is_some_and(|n| glob.is_match(n))
}
