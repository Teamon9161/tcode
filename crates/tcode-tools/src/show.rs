//! `show` — put a file that already exists on screen, beside the conversation.
//!
//! The tool exists because of an economics problem, not a rendering one. A model
//! that wants a human to see a chart has, until now, had to write the chart into
//! its reply: an ```echarts fence carrying every data point, priced per output
//! token and capped by the response limit. For anything derived from a real
//! dataset that is the wrong shape — the data is already on disk, and the model
//! is perfectly capable of writing a script that puts a rendered file there too.
//!
//! So the whole tool is a **path**. Nothing about the file's contents enters the
//! conversation, which is what makes a 500k-row table cost the same as an empty
//! one. Everything downstream — which renderer, how much of it fits, what the
//! reload button does — belongs to the frontend that has a screen.
//!
//! It is deliberately **not** in `builtin_tools()`. A tool whose entire effect is
//! "a pane appears" is a lie in a frontend with no pane, and the zero-guessing
//! rule cuts against handing the model a capability that silently does nothing.
//! The desktop app's composition root registers it (`BootSpec::display_tools`);
//! frontends without a viewer never offer it.

use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use tcode_core::{AutoSafety, BatchPolicy, PermissionRequest, Tool, ToolCtx, ToolOutput};

/// How much of a text file a viewer loads. Shared with the frontend that does
/// the loading, so "the tool warned it would be truncated" and "the viewer
/// truncated it" are the same number rather than two that drift apart.
pub const VIEWER_TEXT_BUDGET: u64 = 4 * 1024 * 1024;

pub struct ShowTool;

#[async_trait]
impl Tool for ShowTool {
    fn name(&self) -> &str {
        "show"
    }

    fn display_name(&self) -> String {
        "Show".into()
    }

    fn description(&self) -> &str {
        "Display a file that already exists on disk beside this conversation, in the app's inspect pane. \
Use this instead of writing a chart, table or HTML document into your reply: write the file first (a script, a query, a plot), then show its path. \
Nothing about the file's contents enters this conversation, so showing a large result costs the same as showing an empty one — never inline a dataset when you can show the file it came from. \
Rendered by extension: .html/.svg as a sandboxed artifact, .mermaid/.mmd as a diagram, .csv/.tsv as a table, .png/.jpg/.gif/.webp as an image, .md as prose, anything else as text."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File to display, absolute or relative to cwd. It must already exist."
                },
                "label": {
                    "type": "string",
                    "description": "Optional short caption for the pane, e.g. 'PnL by month'. Defaults to the file name."
                }
            },
            "required": ["path"]
        })
    }

    fn permission(&self, _input: &Value) -> PermissionRequest {
        PermissionRequest::None
    }

    fn auto_safety(&self, _input: &Value) -> AutoSafety {
        AutoSafety::Allow
    }

    fn batch_policy(&self) -> BatchPolicy {
        BatchPolicy::ParallelReadOnly
    }

    async fn run(&self, input: Value, ctx: &ToolCtx, _cancel: &CancellationToken) -> ToolOutput {
        let Some(raw) = input["path"]
            .as_str()
            .filter(|path| !path.trim().is_empty())
        else {
            return ToolOutput::err("missing required parameter: path");
        };
        let path = ctx.resolve(raw.trim());

        let metadata = match tokio::fs::metadata(&path).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return ToolOutput::err(format!(
                    "no file at {}. `show` displays a file that already exists — create it first (a script that writes the chart, table or report), then show that path.",
                    path.display()
                ));
            }
            Err(error) => {
                return ToolOutput::err(format!("cannot show {}: {error}", path.display()));
            }
        };
        if metadata.is_dir() {
            return ToolOutput::err(format!(
                "{} is a folder; `show` displays one file.",
                path.display()
            ));
        }
        if let Err(reason) = is_viewable_path(&path, &ctx.cwd) {
            return ToolOutput::err(reason);
        }
        if metadata.len() == 0 {
            return ToolOutput::err(format!(
                "{} is empty (0 bytes), so there is nothing to display. If a script was supposed to write it, check that script's output first.",
                path.display()
            ));
        }

        let kind = kind_of(&path);
        let mut line = format!(
            "shown: {} ({kind}, {})",
            path.display(),
            human_bytes(metadata.len())
        );
        if metadata.len() > VIEWER_TEXT_BUDGET {
            line.push_str(
                ". Files this large are truncated in the viewer — show a smaller extract if the whole file matters.",
            );
        }
        ToolOutput::ok(line)
    }
}

/// Where a viewer may read from: this conversation's folder, or tcode's own
/// state directory (scratchpads, tool output, blobs — where generated artifacts
/// naturally land).
///
/// This is not about protecting the model, which can already `read` anywhere.
/// It bounds what the *webview* can ask the backend to load on the strength of
/// a path, and it makes the failure legible instead of a blank pane.
pub fn is_viewable_path(path: &Path, cwd: &Path) -> Result<(), String> {
    if is_within(path, cwd) {
        return Ok(());
    }
    if tcode_core::home_dir()
        .map(|home| is_within(path, &home.join(".tcode")))
        .unwrap_or(false)
    {
        return Ok(());
    }
    Err(format!(
        "{} is outside this session's folder ({}), and only files inside it — or in tcode's own scratch directory — can be displayed. Write the file into the project and show that copy.",
        path.display(),
        cwd.display()
    ))
}

/// Lexical containment. Deliberately not `canonicalize`: it hits the disk for
/// every component, and on Windows it returns `\\?\C:\…`, so one canonicalized
/// side compared against a plain one never matches (the same trap `paths.rs`
/// documents in the app).
///
/// Component-wise rather than by string prefix, so `/proj-secrets` is not
/// "inside" `/proj`. Components are compared case-insensitively on Windows: the
/// model writing `c:\code\…` for a session opened at `C:\code\…` is the same
/// folder there, and refusing it would be a rejection nobody could act on.
pub(crate) fn is_within(path: &Path, root: &Path) -> bool {
    let path = normalize(path);
    let root = normalize(root);
    let mut here = path.components();
    root.components()
        .all(|part| here.next().is_some_and(|mine| same(mine, part)))
}

fn same(left: Component<'_>, right: Component<'_>) -> bool {
    if !cfg!(windows) {
        return left == right;
    }
    left.as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
}

/// Resolves `.` and `..` without touching the filesystem. `..` past the root is
/// dropped rather than escaping, which is the conservative direction: a path
/// that cannot be understood does not become a path outside the root.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The extension, lowercased — the same key the frontend's view registry uses.
/// Reported back so the model can tell "shown as a table" from "shown as text"
/// without a second call.
fn kind_of(path: &Path) -> String {
    path.extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
        .filter(|extension| !extension.is_empty())
        .unwrap_or_else(|| "no extension".into())
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(cwd: &Path) -> ToolCtx {
        ToolCtx::for_test(cwd.to_path_buf(), 25_000)
    }

    async fn run(input: Value, ctx: &ToolCtx) -> ToolOutput {
        ShowTool.run(input, ctx, &CancellationToken::new()).await
    }

    #[tokio::test]
    async fn a_shown_file_reports_kind_and_size_but_never_its_contents() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path());
        std::fs::write(dir.path().join("pnl.csv"), "date,pnl\n2026-01-01,12.5\n").unwrap();

        let out = run(json!({ "path": "pnl.csv" }), &ctx).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("csv"), "{}", out.content);
        assert!(out.content.contains("shown:"), "{}", out.content);
        // The whole point: the file's bytes stay out of the conversation.
        assert!(!out.content.contains("2026-01-01"), "{}", out.content);
    }

    #[tokio::test]
    async fn a_missing_file_says_to_write_it_first() {
        let dir = tempfile::tempdir().unwrap();
        let out = run(json!({ "path": "chart.html" }), &ctx(dir.path())).await;
        assert!(out.is_error);
        assert!(out.content.contains("create it first"), "{}", out.content);
    }

    #[tokio::test]
    async fn an_empty_file_is_an_error_because_it_means_the_script_failed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("out.html"), "").unwrap();
        let out = run(json!({ "path": "out.html" }), &ctx(dir.path())).await;
        assert!(out.is_error);
        assert!(out.content.contains("empty"), "{}", out.content);
    }

    #[tokio::test]
    async fn a_folder_is_refused_with_the_reason() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("reports")).unwrap();
        let out = run(json!({ "path": "reports" }), &ctx(dir.path())).await;
        assert!(out.is_error);
        assert!(out.content.contains("folder"), "{}", out.content);
    }

    #[tokio::test]
    async fn a_file_outside_the_session_folder_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let outside = elsewhere.path().join("secret.html");
        std::fs::write(&outside, "<p>hi</p>").unwrap();

        let out = run(
            json!({ "path": outside.to_string_lossy() }),
            &ctx(dir.path()),
        )
        .await;
        assert!(out.is_error, "{}", out.content);
        assert!(out.content.contains("outside"), "{}", out.content);
    }

    #[tokio::test]
    async fn dot_dot_cannot_walk_out_of_the_session_folder() {
        let parent = tempfile::tempdir().unwrap();
        let cwd = parent.path().join("project");
        std::fs::create_dir(&cwd).unwrap();
        std::fs::write(parent.path().join("above.html"), "<p>hi</p>").unwrap();

        let out = run(json!({ "path": "../above.html" }), &ctx(&cwd)).await;
        assert!(out.is_error, "{}", out.content);
        assert!(out.content.contains("outside"), "{}", out.content);
    }

    #[tokio::test]
    async fn a_large_file_says_the_viewer_will_truncate_it() {
        let dir = tempfile::tempdir().unwrap();
        let big = vec![b'x'; (VIEWER_TEXT_BUDGET + 1) as usize];
        std::fs::write(dir.path().join("big.csv"), big).unwrap();

        let out = run(json!({ "path": "big.csv" }), &ctx(dir.path())).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("truncated"), "{}", out.content);
    }

    /// A sibling whose name merely starts with the folder's name is outside it.
    /// A string prefix test would have said otherwise.
    #[tokio::test]
    async fn a_sibling_folder_with_a_shared_prefix_is_still_outside() {
        let parent = tempfile::tempdir().unwrap();
        let cwd = parent.path().join("proj");
        let sibling = parent.path().join("proj-secrets");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        let outside = sibling.join("keys.html");
        std::fs::write(&outside, "<p>k</p>").unwrap();

        let out = run(json!({ "path": outside.to_string_lossy() }), &ctx(&cwd)).await;
        assert!(out.is_error, "{}", out.content);
        assert!(out.content.contains("outside"), "{}", out.content);
    }

    /// On Windows the same folder is routinely spelled with a different case,
    /// and a refusal the user cannot act on is worse than no check at all.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_differently_cased_spelling_of_the_session_folder_is_the_same_folder() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("plot.html"), "<p>hi</p>").unwrap();
        let shouted = dir.path().to_string_lossy().to_uppercase();

        let out = run(
            json!({ "path": format!("{shouted}\\PLOT.HTML") }),
            &ctx(dir.path()),
        )
        .await;
        assert!(!out.is_error, "{}", out.content);
    }

    #[test]
    fn sizes_read_the_way_a_human_would_say_them() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(3 * 1024 * 1024), "3.0 MB");
    }
}
