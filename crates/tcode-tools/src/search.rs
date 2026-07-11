use std::path::{Path, PathBuf};

use async_trait::async_trait;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::sinks::UTF8;
use grep_searcher::SearcherBuilder;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use tcode_core::{PermissionRequest, Tool, ToolCtx, ToolOutput};

const DEFAULT_MATCH_LIMIT: usize = 200;

fn walk(base: &Path) -> ignore::Walk {
    ignore::WalkBuilder::new(base).hidden(true).build()
}

fn rel_display(path: &Path, base: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .display()
        .to_string()
}

// ---------------------------------------------------------------- grep

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents with a regex (ripgrep engine, respects \
         .gitignore). Returns matching lines as path:line:text. Filter \
         files with `glob`; cap output with head_limit (default 200)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regex to search for" },
                "path": { "type": "string", "description": "Directory or file to search (default: cwd)" },
                "glob": { "type": "string", "description": "Filter files, e.g. *.rs or src/**/*.toml" },
                "case_insensitive": { "type": "boolean" },
                "head_limit": { "type": "integer" }
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
            .unwrap_or(DEFAULT_MATCH_LIMIT);

        let mut searcher = SearcherBuilder::new().line_number(true).build();
        let mut results: Vec<String> = Vec::new();
        let mut truncated = false;
        let mut files_scanned = 0usize;

        for entry in walk(&base) {
            if cancel.is_cancelled() || truncated {
                break;
            }
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if let Some(g) = &glob {
                if !glob_matches(g, path, &base) {
                    continue;
                }
            }
            files_scanned += 1;
            let display = rel_display(path, &ctx.cwd);
            let _ = searcher.search_path(
                &matcher,
                path,
                UTF8(|lnum, line| {
                    results.push(format!("{display}:{lnum}: {}", line.trim_end()));
                    if results.len() >= limit {
                        truncated = true;
                        return Ok(false);
                    }
                    Ok(true)
                }),
            );
        }

        if results.is_empty() {
            return ToolOutput::ok(format!(
                "no matches for /{pattern}/ ({files_scanned} files scanned{})",
                input["glob"]
                    .as_str()
                    .map(|g| format!(", glob {g}"))
                    .unwrap_or_default()
            ));
        }
        let mut out = results.join("\n");
        if truncated {
            out.push_str(&format!(
                "\n[stopped at {limit} matches — narrow the pattern or glob, or raise head_limit]"
            ));
        }
        ToolOutput::ok(out)
    }
}

// ---------------------------------------------------------------- glob

pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        "Find files by name pattern, e.g. **/*.rs or src/**/Cargo.toml. \
         Respects .gitignore. Results sorted by modification time (newest \
         first), capped at 200."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string" },
                "path": { "type": "string", "description": "Base directory (default: cwd)" }
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
        let mut hits: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
        for entry in walk(&base) {
            if cancel.is_cancelled() {
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
            return ToolOutput::ok(format!(
                "no files match {pattern} under {}",
                rel_display(&base, &ctx.cwd)
            ));
        }
        hits.sort_by(|a, b| b.0.cmp(&a.0));
        let total = hits.len();
        hits.truncate(200);
        let mut out: Vec<String> = hits
            .into_iter()
            .map(|(_, p)| rel_display(&p, &ctx.cwd))
            .collect();
        if total > 200 {
            out.push(format!("[{total} matches; showing newest 200]"));
        }
        ToolOutput::ok(out.join("\n"))
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
