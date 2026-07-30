//! Per-tool transcript rendering. `RenderRegistry::from_tools` is the single
//! place in the TUI that matches on tool names; everywhere else consults the
//! registry through the `ToolRenderer` trait. Quiet-output behaviour derives
//! from the live `Tool::batch_policy()`, so it can never drift out of sync
//! with core's parallel-read-only set.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use ratatui::style::Style;
use ratatui::text::Line;
use serde_json::Value;

use tcode_core::{BatchPolicy, Tool};

use crate::diff;
use crate::theme;

/// Where a tool call's rendering goes. The tool decides (`Tool::route`), not
/// this module: every frontend needs the same answer, so it is core's. Re-
/// exported here because the TUI reaches it through this registry.
pub use tcode_core::CallRoute;

/// Colour treatment for a call header. Most tools foreground their verb; a
/// delegated task instead foregrounds its human-authored objective.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderTone {
    Tool,
    Task,
}

/// A batch row is normally quiet, but a delegated task's human-authored
/// objective is its primary label. Its changing status remains dim below.
pub fn batch_item_style(tone: HeaderTone) -> Style {
    match tone {
        HeaderTone::Tool => theme::dim(),
        HeaderTone::Task => Style::default(),
    }
}

pub trait ToolRenderer: Send + Sync {
    fn header_tone(&self) -> HeaderTone {
        HeaderTone::Tool
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

    /// Full content shown in the transcript while this call's approval
    /// dialog is open, so the reviewer reads the whole thing there —
    /// scrollable, as part of the record — rather than in the compact
    /// dialog. Defaults to `body()` (edit/write diffs already qualify).
    /// Shell overrides this because its full command is normally kept
    /// folded (`initial_detail`) to stay out of the way; approval is
    /// exactly when it must be visible before the user decides.
    fn approval_detail(&self, input: &Value) -> Vec<Line<'static>> {
        self.body(input)
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
struct AgentRenderer;

impl ToolRenderer for AgentRenderer {
    fn header_tone(&self) -> HeaderTone {
        HeaderTone::Task
    }

    fn header(&self, _name: &str, input: &Value, _cwd: Option<&Path>) -> String {
        let kind = title_case_tool_name(input["agent"].as_str().unwrap_or("agent"));
        let summary = input["summary"]
            .as_str()
            .or_else(|| {
                input["prompt"]
                    .as_str()
                    .and_then(|prompt| prompt.lines().next())
            })
            .map(str::trim)
            .filter(|summary| !summary.is_empty());
        match summary {
            Some(summary) => format!("{kind} · {summary}"),
            None => kind.to_string(),
        }
    }

    fn batch_item(&self, name: &str, input: &Value, cwd: Option<&Path>) -> String {
        self.header(name, input, cwd)
    }

    fn folds_result(&self, _input: &Value) -> bool {
        true
    }

    fn markdown_detail(&self, _input: Option<&Value>) -> bool {
        true
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

    fn approval_detail(&self, input: &Value) -> Vec<Line<'static>> {
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

struct AppendRenderer;

impl ToolRenderer for AppendRenderer {
    fn body(&self, input: &Value) -> Vec<Line<'static>> {
        diff::append_preview(
            input["path"].as_str().unwrap_or(""),
            input["content"].as_str().unwrap_or(""),
        )
    }

    fn error_label(&self) -> Option<&'static str> {
        Some("Append(error)")
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

    /// The vision model's answer is prose, not a single-line value — fold it
    /// under the header like `task`/`web_fetch` instead of gluing it onto the
    /// call's own line.
    fn folds_result(&self, _input: &Value) -> bool {
        true
    }

    fn markdown_detail(&self, _input: Option<&Value>) -> bool {
        true
    }
}

/// `progress` wears two faces, and which one shows is a property of the call
/// rather than of the tool: an ordinary update feeds the live phase pane, while
/// a plan submitted for the user's approval is a document they need to read, so
/// it renders into the transcript as markdown. The same block appears live
/// (baked while the review dialog is open) and on replay (from the ledgered
/// call), so both paths go through here.
struct ProgressRenderer;

/// The markdown of a plan submitted for approval, or `None` for an ordinary
/// progress update. Core's, because the same question decides where the call is
/// routed (`Tool::route`) — "is this a document" must have one answer.
fn submitted_plan(input: &Value) -> Option<String> {
    tcode_core::progress::plan_document(input)
}

impl ToolRenderer for ProgressRenderer {
    fn header(&self, name: &str, input: &Value, cwd: Option<&Path>) -> String {
        if submitted_plan(input).is_none() {
            return tcode_core::agent::summarize_call(name, input);
        }
        let _ = cwd;
        match input["title"]
            .as_str()
            .map(str::trim)
            .filter(|title| !title.is_empty())
        {
            Some(title) => format!("Proposed plan: {title}"),
            None => "Proposed plan".to_string(),
        }
    }

    /// Folded, not inline. Once the plan is approved it also lives in the
    /// progress pane, which is where the phase list belongs; repeating it in
    /// the transcript as a column of accent headings says nothing new and
    /// drowns out the reasoning. Folded, the header alone marks the moment,
    /// and opening it gives the whole document.
    fn initial_detail(&self, input: &Value) -> Vec<Line<'static>> {
        plan_block(input)
    }

    /// The one moment the reviewer must see it without asking.
    fn approval_detail(&self, input: &Value) -> Vec<Line<'static>> {
        plan_block(input)
    }
}

fn plan_block(input: &Value) -> Vec<Line<'static>> {
    match submitted_plan(input) {
        Some(plan) => crate::markdown::Renderer::default().render(&demote_headings(plan.trim())),
        None => Vec::new(),
    }
}

/// Push every heading down one level for display. A phase is a row in a
/// document here, not a section of one: rendered at their authored depth the
/// phase titles take the accent-and-underline treatment reserved for the top
/// of a document, and a six-phase plan becomes six banners with the reasoning
/// between them as an afterthought. The file on disk keeps its own levels —
/// this is presentation, and the plan is read as a whole.
fn demote_headings(markdown: &str) -> String {
    let mut out = String::with_capacity(markdown.len() + 16);
    let mut fenced = false;
    for line in markdown.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
        }
        if !fenced && trimmed.starts_with('#') {
            out.push('#');
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// A tool whose story is told by another surface still needs a renderer entry,
/// because `header` is what the plan pane and the task tree label it with.
struct SilentRenderer;

impl ToolRenderer for SilentRenderer {}

pub struct RenderRegistry {
    renderers: HashMap<String, Box<dyn ToolRenderer>>,
    /// Tool name → UI display name, snapshotted from the live tools.
    display_names: HashMap<String, String>,
    /// Tool name → the live tool, kept solely to ask it where a call goes.
    /// Routing is the tool's own answer (`Tool::route`); holding the tool is how
    /// this registry asks without owning a second opinion about tool names.
    routes: HashMap<String, Arc<dyn Tool>>,
    /// Imported or since-unregistered tools render generically.
    fallback: DefaultRenderer,
}

impl RenderRegistry {
    pub fn from_tools(tools: &[Arc<dyn Tool>]) -> Self {
        let mut renderers: HashMap<String, Box<dyn ToolRenderer>> = HashMap::new();
        let mut display_names = HashMap::new();
        let mut routes: HashMap<String, Arc<dyn Tool>> = HashMap::new();
        for tool in tools {
            let name = tool.name();
            display_names.insert(name.to_string(), tool.display_name());
            let quiet = tool.batch_policy() == BatchPolicy::ParallelReadOnly;
            let renderer: Box<dyn ToolRenderer> = match name {
                "shell" | "bash" | "monitor" => Box::new(ShellRenderer),
                "edit" => Box::new(EditRenderer),
                "write" => Box::new(WriteRenderer),
                "append" => Box::new(AppendRenderer),
                "read" => Box::new(ReadRenderer { quiet }),
                "grep" | "glob" => Box::new(PatternRenderer { quiet }),
                "web_fetch" => Box::new(WebFetchRenderer),
                "view_image" => Box::new(ViewImageRenderer),
                "agent" => Box::new(AgentRenderer),
                "progress" => Box::new(ProgressRenderer),
                "ask_user" => Box::new(SilentRenderer),
                _ => Box::new(DefaultRenderer { quiet }),
            };
            renderers.insert(name.to_string(), renderer);
            routes.insert(name.to_string(), tool.clone());
        }
        // Existing JSONL sessions retain retired tool names and schemas. Keep
        // their specialized renderers on resume without exposing those names
        // to new model requests.
        if renderers.contains_key("agent") {
            renderers.insert("task".into(), Box::new(AgentRenderer));
            display_names.insert("task".into(), "Agent".into());
        }
        // `update_plan`/`update_progress` are this tool's earlier names and
        // `exit_plan` its earlier plan-submission half; all three route by
        // input exactly like the current one does — including through the live
        // tool, which is why the alias lands in `routes` too.
        if renderers.contains_key("progress") {
            let progress = routes.get("progress").cloned();
            for retired in ["update_plan", "update_progress", "exit_plan"] {
                renderers.insert(retired.into(), Box::new(ProgressRenderer));
                if let Some(tool) = progress.clone() {
                    routes.insert(retired.into(), tool);
                }
            }
        }
        Self {
            renderers,
            display_names,
            routes,
            fallback: DefaultRenderer { quiet: false },
        }
    }

    /// Where a call goes, straight from the tool that will run it. `progress`
    /// decides from its input, so pass it when it is still at hand; a call whose
    /// input has already been consumed asks with `Null` and gets the tool's
    /// default answer, which is what the header path chose for it in the first
    /// place. A tool this session does not have (an imported or since-removed
    /// one) renders in the transcript.
    pub fn route_of(&self, name: &str, input: Option<&Value>) -> CallRoute {
        let Some(tool) = self.routes.get(name) else {
            return CallRoute::Transcript;
        };
        tool.route(input.unwrap_or(&Value::Null))
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

    struct AgentStub;

    #[async_trait::async_trait]
    impl Tool for AgentStub {
        fn name(&self) -> &str {
            "agent"
        }

        fn description(&self) -> &str {
            "test agent"
        }

        fn input_schema(&self) -> Value {
            json!({"type": "object"})
        }

        fn permission(&self, _input: &Value) -> tcode_core::PermissionRequest {
            tcode_core::PermissionRequest::None
        }

        async fn run(
            &self,
            _input: Value,
            _ctx: &tcode_core::ToolCtx,
            _cancel: &tokio_util::sync::CancellationToken,
        ) -> tcode_core::ToolOutput {
            tcode_core::ToolOutput::ok("")
        }
    }

    #[test]
    fn agent_renderer_keeps_a_task_alias_for_legacy_replay() {
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(AgentStub)];
        let registry = RenderRegistry::from_tools(&tools);
        let input = json!({"agent": "explore", "prompt": "survey"});
        assert_eq!(registry.get("agent").header_tone(), HeaderTone::Task);
        assert_eq!(registry.get("task").header_tone(), HeaderTone::Task);
        assert!(registry.get("task").folds_result(&input));
        assert!(registry.get("task").markdown_detail(Some(&input)));
        assert_eq!(registry.display_name("task"), "Agent");
    }

    #[test]
    fn quiet_output_tracks_core_parallel_read_only_policy() {
        let registry = registry();
        for name in ["read", "grep", "glob"] {
            assert!(registry.get(name).quiet_output(), "{name} should be quiet");
        }
        for name in [
            "shell",
            "edit",
            "write",
            "append",
            "web_fetch",
            "not-a-tool",
        ] {
            assert!(
                !registry.get(name).quiet_output(),
                "{name} should not be quiet"
            );
        }
    }

    /// Routing is the tool's answer, so the registry has to be asked with the
    /// live tools rather than with a renderer of its own.
    #[test]
    fn routes_split_progress_and_silent_tools_from_the_transcript() {
        let empty = RenderRegistry::from_tools(&[]);
        assert_eq!(empty.route_of("unknown", None), CallRoute::Transcript);
        // A tool this session does not have renders in the transcript; nothing
        // else could be shown for a call whose surface is unknown.
        assert_eq!(empty.route_of("progress", None), CallRoute::Transcript);

        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(tcode_tools::ProgressTool),
            Arc::new(tcode_tools::AskUserTool),
        ];
        let registry = RenderRegistry::from_tools(&tools);
        assert_eq!(registry.route_of("ask_user", None), CallRoute::Silent);
        // Retired names route through the live tool, so resumed sessions still
        // send old progress calls to the plan surface.
        for name in ["progress", "update_progress", "update_plan", "exit_plan"] {
            assert_eq!(registry.route_of(name, None), CallRoute::Progress, "{name}");
        }
    }

    /// One tool, two renderings: a phase flip belongs in the live pane, a plan
    /// submitted for approval is a document the user reads in the transcript.
    #[test]
    fn a_submitted_plan_routes_to_the_transcript_and_an_update_does_not() {
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(tcode_tools::ProgressTool)];
        let registry = RenderRegistry::from_tools(&tools);
        let update = json!({ "phases": [{ "phase": "one", "status": "in_progress" }] });
        assert_eq!(
            registry.route_of("progress", Some(&update)),
            CallRoute::Progress
        );

        let submission = json!({
            "title": "Rewrite the resume path",
            "state": "active",
            "phases": [{ "phase": "one", "status": "pending" }]
        });
        assert_eq!(
            registry.route_of("progress", Some(&submission)),
            CallRoute::Transcript
        );
        assert!(ProgressRenderer
            .header("progress", &submission, None)
            .contains("Rewrite the resume path"));
        // The plan is foldable, not inline: the phase list is the progress
        // pane's job, and the reviewer still gets the whole document while the
        // approval dialog is open.
        assert!(ProgressRenderer.body(&submission).is_empty());
        assert!(!ProgressRenderer.initial_detail(&submission).is_empty());
        assert!(!ProgressRenderer.approval_detail(&submission).is_empty());

        // Retired `exit_plan` calls carry the body itself, not phases.
        let legacy = json!({ "plan": "# Do it

body" });
        assert_eq!(
            registry.route_of("exit_plan", Some(&legacy)),
            CallRoute::Transcript
        );
    }

    #[test]
    fn plan_headings_are_demoted_for_display_but_fences_are_left_alone() {
        let plan = "## [ ] 1. One\nwhy\n\n```\n# not a heading\n```\n";
        let shown = demote_headings(plan);
        assert!(shown.contains("### [ ] 1. One"), "{shown}");
        assert!(shown.contains("\n# not a heading\n"), "{shown}");
    }

    #[test]
    fn task_header_shows_kind_and_parent_authored_summary() {
        let renderer = AgentRenderer;
        let explore = json!({
            "agent": "explore",
            "summary": "inspect TUI rendering",
            "prompt": "longer fallback prompt"
        });
        assert_eq!(
            renderer.header("task", &explore, None),
            "Explore · inspect TUI rendering"
        );
        assert_eq!(renderer.header_tone(), HeaderTone::Task);
        assert_eq!(batch_item_style(renderer.header_tone()), Style::default());
        assert_eq!(
            renderer.batch_item("task", &explore, None),
            "Explore · inspect TUI rendering",
            "parallel explore calls use the same title as a single call"
        );
        assert_eq!(
            renderer.header(
                "task",
                &json!({"agent": "plan", "prompt": "draft a migration plan\nwith details"}),
                None,
            ),
            "Plan · draft a migration plan"
        );
    }

    #[test]
    fn view_image_folds_its_result_instead_of_gluing_it_onto_the_header() {
        let renderer = ViewImageRenderer;
        let input = json!({ "paths": ["shot.png"], "prompt": "describe" });
        // Regression: the vision model's answer used to have no opt-out from
        // the default same-line preview, so it got appended to the header row.
        assert!(renderer.folds_result(&input));
        assert!(renderer.markdown_detail(Some(&input)));
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

    /// A long/multi-line command must be readable in full while its approval
    /// dialog is open (the dialog itself only shows a capped header), so
    /// `approval_detail` — unlike `body`, which batch items also render and
    /// which must stay compact — surfaces the same block `initial_detail`
    /// folds behind the header.
    #[test]
    fn shell_approval_detail_surfaces_the_full_command_but_body_stays_empty() {
        let renderer = ShellRenderer;
        let short = json!({ "command": "git status" });
        assert!(renderer.approval_detail(&short).is_empty());
        assert!(renderer.body(&short).is_empty());

        let multiline = json!({ "command": "a\nb" });
        assert!(!renderer.approval_detail(&multiline).is_empty());
        assert_eq!(
            renderer.approval_detail(&multiline),
            renderer.initial_detail(&multiline)
        );
        // Batch items render `body`, not `approval_detail` — a batch of shell
        // calls must stay compact even when one command is long.
        assert!(renderer.body(&multiline).is_empty());
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
            .syntax_detail(&plain, "let answer = 42;\n")
            .expect("read source has syntax detail");
        let rendered: String = syntax[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(rendered.ends_with("let answer = 42;"));
        assert!(
            syntax[0].spans[1].content.trim().is_empty(),
            "raw read output does not invent a line number"
        );
        assert!(syntax[0].spans.iter().any(|span| span.style.fg.is_some()));
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
