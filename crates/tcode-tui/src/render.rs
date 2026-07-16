//! Per-tool transcript rendering. `RenderRegistry::from_tools` is the single
//! place in the TUI that matches on tool names; everywhere else consults the
//! registry through the `ToolRenderer` trait. Quiet-output behaviour derives
//! from the live `Tool::batch_policy()`, so it can never drift out of sync
//! with core's parallel-read-only set.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use ratatui::text::Line;
use serde_json::Value;

use tcode_core::{BatchPolicy, Tool};

use crate::diff;

/// Where a tool call's rendering goes. `Progress` feeds the execution-progress
/// pane instead of the transcript (`update_progress`); `Silent` renders nothing
/// because another mechanism already told the story (ask_user's approval record).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallRoute {
    Transcript,
    Progress,
    Silent,
}

pub trait ToolRenderer: Send + Sync {
    fn route(&self) -> CallRoute {
        CallRoute::Transcript
    }

    /// One-line header text (uncolored; the App applies display-name
    /// coloring). Long/multi-line shell commands use a capped first-line
    /// preview while their full command remains folded below.
    fn header(&self, name: &str, input: &Value, _cwd: Option<&Path>) -> String {
        tcode_core::agent::summarize_call(name, input)
    }

    /// Detail available as soon as a single call starts. Long shell commands
    /// use this so their full text stays folded until explicitly opened.
    /// Batch items deliberately remain compact.
    fn initial_detail(&self, _input: &Value) -> Vec<Line<'static>> {
        Vec::new()
    }

    /// Whether a call's result belongs in its foldout without a visible
    /// preview. Shell commands use this so their output never competes with
    /// the command summary, regardless of command length.
    fn folds_result(&self, _input: &Value) -> bool {
        false
    }

    /// Render a successful result body as syntax-highlighted source. The
    /// caller keeps errors literal so diagnostics never masquerade as code.
    fn syntax_detail(&self, _input: &Value, _content: &str) -> Option<Vec<Line<'static>>> {
        None
    }

    /// A concise error label for a result whose complete diagnostic is kept in
    /// the foldout. The call body remains visible as the attempted change.
    fn error_label(&self) -> Option<&'static str> {
        None
    }

    /// Change preview under the header (edit diff / write content). Rendered
    /// for single calls, batch items and approval prebakes alike.
    fn body(&self, _input: &Value) -> Vec<Line<'static>> {
        Vec::new()
    }

    /// Compact text for an indented batch row.
    fn batch_item(&self, name: &str, input: &Value, cwd: Option<&Path>) -> String {
        shorten_summary_path(&tcode_core::agent::summarize_call(name, input), cwd)
    }

    /// Successful results keep only the fold affordance, no preview line:
    /// derived from core's `BatchPolicy::ParallelReadOnly` at registration.
    fn quiet_output(&self) -> bool {
        false
    }

    /// Successful results render nothing at all — the body already told the
    /// story at the call site (edit/write diffs). Errors still surface.
    fn hide_success_result(&self) -> bool {
        false
    }

    /// Render the foldable result body as markdown (web_fetch; read of a
    /// markdown file) instead of literal text.
    fn markdown_detail(&self, _input: Option<&Value>) -> bool {
        false
    }
}

struct DefaultRenderer {
    quiet: bool,
}

impl ToolRenderer for DefaultRenderer {
    fn quiet_output(&self) -> bool {
        self.quiet
    }
}

/// A sub-agent's report is prose the model wrote for a human to read — it
/// arrives as Markdown and must not be shown as literal `#` and `**`.
struct TaskRenderer;

impl ToolRenderer for TaskRenderer {
    fn markdown_detail(&self, _input: Option<&Value>) -> bool {
        true
    }
}

/// The plan a model submits with `exit_plan`: a heading plus the plan body as
/// rendered markdown. The same block appears live (baked while the review
/// dialog is open) and on replay (from the ledgered call), so both paths must
/// go through here.
struct ExitPlanRenderer;

impl ToolRenderer for ExitPlanRenderer {
    fn header(&self, _name: &str, input: &Value, _cwd: Option<&Path>) -> String {
        match input["title"]
            .as_str()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            Some(title) => format!("Proposed plan: {title}"),
            None => "Proposed plan".to_string(),
        }
    }

    fn body(&self, input: &Value) -> Vec<Line<'static>> {
        let plan = input["plan"].as_str().unwrap_or("").trim();
        if plan.is_empty() {
            return Vec::new();
        }
        crate::markdown::Renderer::default().render(plan)
    }
}

struct ShellRenderer;

/// Header previews must leave room for the tool label, result affordance, and
/// common terminal widths. The full command remains in the foldout.
const SHELL_HEADER_PREVIEW_MAX: usize = 56;

impl ToolRenderer for ShellRenderer {
    fn header(&self, name: &str, input: &Value, _cwd: Option<&Path>) -> String {
        let command = command_of(input);
        if diff::command_is_block(command) {
            format!("{name}({})", command_header_preview(command))
        } else {
            tcode_core::agent::summarize_call(name, input)
        }
    }

    fn initial_detail(&self, input: &Value) -> Vec<Line<'static>> {
        diff::command_block(command_of(input))
    }

    fn folds_result(&self, _input: &Value) -> bool {
        true
    }

    fn batch_item(&self, name: &str, input: &Value, _cwd: Option<&Path>) -> String {
        command_first_line(input["command"].as_str().unwrap_or(name))
    }
}

struct EditRenderer;

impl ToolRenderer for EditRenderer {
    fn body(&self, input: &Value) -> Vec<Line<'static>> {
        diff::edit_diff(
            input["path"].as_str().unwrap_or(""),
            input["old_string"].as_str().unwrap_or(""),
            input["new_string"].as_str().unwrap_or(""),
        )
    }

    fn error_label(&self) -> Option<&'static str> {
        Some("Edit(error)")
    }

    fn batch_item(&self, name: &str, input: &Value, cwd: Option<&Path>) -> String {
        file_target_item(name, input, cwd)
    }

    fn hide_success_result(&self) -> bool {
        true
    }
}

struct WriteRenderer;

impl ToolRenderer for WriteRenderer {
    fn body(&self, input: &Value) -> Vec<Line<'static>> {
        diff::write_preview(
            input["path"].as_str().unwrap_or(""),
            input["content"].as_str().unwrap_or(""),
        )
    }

    fn batch_item(&self, name: &str, input: &Value, cwd: Option<&Path>) -> String {
        file_target_item(name, input, cwd)
    }

    fn hide_success_result(&self) -> bool {
        true
    }
}

struct ReadRenderer {
    quiet: bool,
}

impl ToolRenderer for ReadRenderer {
    fn batch_item(&self, _name: &str, input: &Value, cwd: Option<&Path>) -> String {
        let path = input_path(input)
            .map(|path| shorten_path(path, cwd))
            .unwrap_or_else(|| "<missing path>".into());
        let offset = input["offset"].as_u64().unwrap_or(1);
        match input["limit"].as_u64() {
            Some(limit) => format!("{path}:{offset}-{}", offset + limit - 1),
            None if offset > 1 => format!("{path}:{offset}-"),
            None => path,
        }
    }

    fn quiet_output(&self) -> bool {
        self.quiet
    }

    fn syntax_detail(&self, input: &Value, content: &str) -> Option<Vec<Line<'static>>> {
        input_path(input).map(|path| diff::read_preview(path, content))
    }

    fn markdown_detail(&self, input: Option<&Value>) -> bool {
        input.is_some_and(path_is_markdown)
    }
}

/// grep / glob: the pattern is the story.
struct PatternRenderer {
    quiet: bool,
}

impl ToolRenderer for PatternRenderer {
    fn header(&self, name: &str, input: &Value, cwd: Option<&Path>) -> String {
        // Core's generic summary would show the *search root* here: `path`
        // outranks `pattern` in its key order, and real calls nearly always
        // carry one. The pattern is what the reader needs; the root is a
        // qualifier.
        let Some(pattern) = input["pattern"].as_str().filter(|p| !p.is_empty()) else {
            return name.to_string();
        };
        match input["path"]
            .as_str()
            .map(|path| shorten_path(path, cwd))
            .filter(|path| !path.is_empty() && path != ".")
        {
            Some(path) => format!("{name}({pattern} in {path})"),
            None => format!("{name}({pattern})"),
        }
    }

    fn batch_item(&self, name: &str, input: &Value, _cwd: Option<&Path>) -> String {
        input["pattern"].as_str().unwrap_or(name).to_string()
    }

    fn quiet_output(&self) -> bool {
        self.quiet
    }
}

struct WebFetchRenderer;

impl ToolRenderer for WebFetchRenderer {
    fn markdown_detail(&self, _input: Option<&Value>) -> bool {
        true
    }
}

struct ViewImageRenderer;

impl ToolRenderer for ViewImageRenderer {
    fn header(&self, name: &str, input: &Value, cwd: Option<&Path>) -> String {
        let paths = input["paths"].as_array();
        let first = paths
            .and_then(|paths| paths.first())
            .and_then(Value::as_str)
            .map(|path| shorten_path(path, cwd))
            .unwrap_or_else(|| "<missing image>".into());
        let extra = paths.map_or(0, |paths| paths.len().saturating_sub(1));
        let prompt: String = input["prompt"]
            .as_str()
            .unwrap_or("")
            .chars()
            .take(40)
            .collect();
        let images = if extra == 0 {
            first
        } else {
            format!("{first} (+{extra} more)")
        };
        if prompt.is_empty() {
            format!("{name}({images})")
        } else {
            format!("{name}({images}: \"{prompt}\")")
        }
    }

    fn batch_item(&self, _name: &str, input: &Value, cwd: Option<&Path>) -> String {
        self.header("", input, cwd)
            .trim_start_matches('(')
            .trim_end_matches(')')
            .to_string()
    }
}

struct ProgressRenderer;

impl ToolRenderer for ProgressRenderer {
    fn route(&self) -> CallRoute {
        CallRoute::Progress
    }
}

struct SilentRenderer;

impl ToolRenderer for SilentRenderer {
    fn route(&self) -> CallRoute {
        CallRoute::Silent
    }
}

pub struct RenderRegistry {
    renderers: HashMap<String, Box<dyn ToolRenderer>>,
    /// Tool name → UI display name, snapshotted from the live tools.
    display_names: HashMap<String, String>,
    /// Imported or since-unregistered tools render generically.
    fallback: DefaultRenderer,
}

impl RenderRegistry {
    pub fn from_tools(tools: &[Arc<dyn Tool>]) -> Self {
        let mut renderers: HashMap<String, Box<dyn ToolRenderer>> = HashMap::new();
        let mut display_names = HashMap::new();
        for tool in tools {
            let name = tool.name();
            display_names.insert(name.to_string(), tool.display_name());
            let quiet = tool.batch_policy() == BatchPolicy::ParallelReadOnly;
            let renderer: Box<dyn ToolRenderer> = match name {
                "shell" | "bash" => Box::new(ShellRenderer),
                "edit" => Box::new(EditRenderer),
                "write" => Box::new(WriteRenderer),
                "read" => Box::new(ReadRenderer { quiet }),
                "grep" | "glob" => Box::new(PatternRenderer { quiet }),
                "web_fetch" => Box::new(WebFetchRenderer),
                "view_image" => Box::new(ViewImageRenderer),
                "task" => Box::new(TaskRenderer),
                "update_progress" => Box::new(ProgressRenderer),
                "ask_user" => Box::new(SilentRenderer),
                "exit_plan" => Box::new(ExitPlanRenderer),
                _ => Box::new(DefaultRenderer { quiet }),
            };
            renderers.insert(name.to_string(), renderer);
        }
        // Existing JSONL sessions retain the retired call name and schema. Keep
        // their progress pane visible on resume without exposing that name to
        // new model requests.
        if renderers.contains_key("update_progress") {
            renderers.insert("update_plan".into(), Box::new(ProgressRenderer));
        }
        Self {
            renderers,
            display_names,
            fallback: DefaultRenderer { quiet: false },
        }
    }

    pub fn get(&self, name: &str) -> &dyn ToolRenderer {
        self.renderers
            .get(name)
            .map(|r| r.as_ref())
            .unwrap_or(&self.fallback)
    }

    /// Tool's UI name, resolved from its own `display_name` when it belongs
    /// to this session; falls back to title-case for imported/unknown tools.
    pub fn display_name(&self, name: &str) -> String {
        self.display_names
            .get(name)
            .cloned()
            .unwrap_or_else(|| title_case_tool_name(name))
    }
}

fn command_of(input: &Value) -> &str {
    input["command"].as_str().unwrap_or("")
}

fn file_target_item(name: &str, input: &Value, cwd: Option<&Path>) -> String {
    input_path(input)
        .map(|path| shorten_path(path, cwd))
        .unwrap_or_else(|| name.to_string())
}

/// First physical line shown in a folded long command's header. An ellipsis
/// marks both a clipped long line and the existence of later command lines.
fn command_header_preview(command: &str) -> String {
    let first = command.lines().next().unwrap_or("").trim();
    if first.is_empty() {
        return "…".to_string();
    }
    let mut preview: String = first.chars().take(SHELL_HEADER_PREVIEW_MAX).collect();
    if first.chars().count() > SHELL_HEADER_PREVIEW_MAX || command.lines().nth(1).is_some() {
        preview.push('…');
    }
    preview
}

/// First line of a command, capped, with a note when more lines follow. Keeps
/// a multi-line command from corrupting a compact one-line batch row.
fn command_first_line(cmd: &str) -> String {
    let mut line = cmd.lines().next().unwrap_or("").to_string();
    if line.chars().count() > 120 {
        line = line.chars().take(120).collect::<String>() + "…";
    }
    let extra = cmd.lines().count().saturating_sub(1);
    if extra > 0 {
        line.push_str(&format!(" (+{extra} lines)"));
    }
    line
}

/// `path`/`file_path` covers native and imported (Claude Code) call shapes.
pub(crate) fn input_path(input: &Value) -> Option<&str> {
    input["path"]
        .as_str()
        .or_else(|| input["file_path"].as_str())
}

/// Whether a `read` call targets a Markdown file (so its output is worth
/// rendering rather than showing raw).
fn path_is_markdown(input: &Value) -> bool {
    input_path(input)
        .map(|p| p.rsplit('.').next().unwrap_or("").to_ascii_lowercase())
        .is_some_and(|ext| matches!(ext.as_str(), "md" | "markdown" | "mdx"))
}

/// Fallback UI name for a tool whose handle we no longer hold: title-case.
fn title_case_tool_name(name: &str) -> String {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    first.to_uppercase().collect::<String>() + chars.as_str()
}

pub(crate) fn shorten_path(path: &str, cwd: Option<&Path>) -> String {
    let Some(cwd) = cwd else {
        return path.to_string();
    };
    Path::new(path)
        .strip_prefix(cwd)
        .map(|relative| relative.display().to_string())
        .unwrap_or_else(|_| path.to_string())
}

/// Tool inputs are canonical absolute paths, but repeating the current
/// project root adds noise without adding information in the TUI.
pub(crate) fn shorten_summary_path(summary: &str, cwd: Option<&Path>) -> String {
    let Some(cwd) = cwd else {
        return summary.to_string();
    };
    let Some((tool, argument)) = summary.split_once('(') else {
        return summary.to_string();
    };
    let Some(argument) = argument.strip_suffix(')') else {
        return summary.to_string();
    };
    let Ok(relative) = Path::new(argument).strip_prefix(cwd) else {
        return summary.to_string();
    };
    format!("{tool}({})", relative.display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn registry() -> RenderRegistry {
        RenderRegistry::from_tools(&tcode_tools::builtin_tools(&std::env::temp_dir()))
    }

    /// The machine-checked replacement for the old "keep this list in sync
    /// with core's ParallelReadOnly set by hand" comment.
    #[test]
    fn quiet_output_tracks_core_parallel_read_only_policy() {
        let registry = registry();
        for name in ["read", "grep", "glob"] {
            assert!(registry.get(name).quiet_output(), "{name} should be quiet");
        }
        for name in ["shell", "edit", "write", "web_fetch", "not-a-tool"] {
            assert!(
                !registry.get(name).quiet_output(),
                "{name} should not be quiet"
            );
        }
    }

    #[test]
    fn routes_split_progress_and_silent_tools_from_the_transcript() {
        let registry = RenderRegistry::from_tools(&[]);
        assert_eq!(registry.get("unknown").route(), CallRoute::Transcript);
        assert_eq!(ProgressRenderer.route(), CallRoute::Progress);
        assert_eq!(SilentRenderer.route(), CallRoute::Silent);

        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(tcode_tools::UpdateProgressTool)];
        let registry = RenderRegistry::from_tools(&tools);
        assert_eq!(registry.get("update_progress").route(), CallRoute::Progress);
        assert_eq!(registry.get("update_plan").route(), CallRoute::Progress);
    }

    #[test]
    fn shell_headers_preview_only_long_commands_and_always_fold_results() {
        let renderer = ShellRenderer;
        let short = json!({ "command": "git status" });
        assert_eq!(renderer.header("shell", &short, None), "shell(git status)");
        assert!(renderer.initial_detail(&short).is_empty());
        assert!(renderer.folds_result(&short));

        let multiline = json!({ "command": "a\nb" });
        assert_eq!(renderer.header("shell", &multiline, None), "shell(a…)");
        assert!(!renderer.initial_detail(&multiline).is_empty());
        assert!(renderer.folds_result(&multiline));

        let long = json!({ "command": format!("echo {}", "x".repeat(100)) });
        let long_header = renderer.header("bash", &long, None);
        assert!(long_header.starts_with("bash(echo "));
        assert!(long_header.ends_with("…)"));
        assert!(long_header.chars().count() <= "bash(".len() + SHELL_HEADER_PREVIEW_MAX + 2);
        assert!(renderer.folds_result(&long));
    }

    /// Real grep calls almost always carry a `path`, which outranks `pattern`
    /// in core's generic summary — the header would say where it searched and
    /// never what for.
    #[test]
    fn search_header_leads_with_the_pattern_not_the_search_root() {
        let renderer = PatternRenderer { quiet: true };
        let cwd = Path::new("/home/me/proj");

        let scoped = json!({ "pattern": "TODO", "path": "/home/me/proj/crates" });
        assert_eq!(
            renderer.header("grep", &scoped, Some(cwd)),
            "grep(TODO in crates)"
        );

        let whole_tree = json!({ "pattern": "TODO", "path": "." });
        assert_eq!(
            renderer.header("grep", &whole_tree, Some(cwd)),
            "grep(TODO)"
        );

        let bare = json!({ "pattern": "**/*.rs" });
        assert_eq!(renderer.header("glob", &bare, Some(cwd)), "glob(**/*.rs)");
    }

    #[test]
    fn edit_and_write_render_bodies_and_hide_successful_results() {
        let edit = json!({
            "path": "src/main.rs", "old_string": "let x = 1;", "new_string": "let x = 2;"
        });
        assert!(!EditRenderer.body(&edit).is_empty());
        assert_eq!(EditRenderer.error_label(), Some("Edit(error)"));
        assert!(EditRenderer.hide_success_result());

        let write = json!({ "path": "src/new.rs", "content": "fn main() {}\n" });
        assert!(!WriteRenderer.body(&write).is_empty());
        assert!(WriteRenderer.hide_success_result());
    }

    #[test]
    fn read_batch_items_show_path_and_range() {
        let renderer = ReadRenderer { quiet: true };
        let cwd = Path::new("/work");
        let plain = json!({ "path": "/work/a.rs" });
        assert_eq!(renderer.batch_item("read", &plain, Some(cwd)), "a.rs");
        let syntax = renderer
            .syntax_detail(&plain, "     1\tlet answer = 42;\n")
            .expect("read source has syntax detail");
        assert!(syntax[0]
            .spans
            .iter()
            .skip(3)
            .any(|span| span.style.fg.is_some()));
        let ranged = json!({ "path": "/work/a.rs", "offset": 10, "limit": 5 });
        assert_eq!(
            renderer.batch_item("read", &ranged, Some(cwd)),
            "a.rs:10-14"
        );
        assert!(renderer.markdown_detail(Some(&json!({ "path": "doc.md" }))));
        assert!(!renderer.markdown_detail(Some(&json!({ "path": "doc.rs" }))));
    }

    #[test]
    fn project_paths_are_shortened_but_other_arguments_are_unchanged() {
        let cwd = Path::new("/work/tcode");
        assert_eq!(
            shorten_summary_path("read(/work/tcode/crates/core.rs)", Some(cwd)),
            "read(crates/core.rs)"
        );
        assert_eq!(
            shorten_summary_path("shell(cargo test)", Some(cwd)),
            "shell(cargo test)"
        );
        assert_eq!(
            shorten_summary_path("read(/tmp/other.rs)", Some(cwd)),
            "read(/tmp/other.rs)"
        );
    }
}
