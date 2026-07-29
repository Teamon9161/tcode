mod agent;
mod frontmatter;
mod fs;
mod grounding;
mod interaction;
mod mcp;
mod monitor;
mod progress;
mod redact;
mod search;
mod shell;
mod shell_filter;
mod show;
mod skills;
mod view_image;
mod web;

pub use agent::cohort::CohortTool;
pub use agent::defs::{
    keeps_tool, AgentDef, AgentModelHint, AgentRegistry, AgentSource, ToolPolicy, ToolSelector,
    MAX_TASK_DEPTH,
};
pub use agent::AgentTool;
pub use grounding::{
    environment_snapshot, project_map, project_map_with_scratch, startup_context_with_scratch,
};
pub use interaction::{AddNoteTool, AskUserTool};
pub use mcp::connect_mcp_servers;
pub use progress::ProgressTool;
pub use shell::ShellKind;
pub use shell_filter::{OutputFilter, ShellFilters};
/// Not part of [`builtin_tools`]: only a frontend with a viewer can honour it.
/// See `show.rs` and `BootSpec::display_tools`. `is_viewable_path` and
/// `VIEWER_TEXT_BUDGET` are the same boundary and the same budget the viewer
/// must apply — one definition, used by the tool and by whoever loads the file.
pub use show::{is_viewable_path, ShowTool, VIEWER_TEXT_BUDGET};
pub use skills::{
    discover_skills, parse_skill_echo, render_skill, wrap_skill_echo, Skill, SkillEcho,
    SkillSource, SkillTool,
};
pub use view_image::ViewImageTool;
pub use web::{trusted_read_hosts, FetchSummarizer, TrustedReadHosts, WebFetchTool};

/// The primary command interpreter on this platform. Windows uses PowerShell;
/// Unix uses Bash.
pub const fn primary_shell_kind() -> ShellKind {
    if cfg!(windows) {
        ShellKind::PowerShell
    } else {
        ShellKind::Bash
    }
}

/// The command tool registered for [`primary_shell_kind`]. Consumers that need
/// to address it should use this instead of duplicating target-specific names.
pub const fn primary_shell_tool_name() -> &'static str {
    primary_shell_kind().tool_name()
}

use std::path::Path;
use std::sync::Arc;

use tcode_core::Tool;

/// Built-in toolset. On Windows PowerShell is the primary shell and a
/// `bash` tool appears when Git Bash is on PATH; on Unix there is bash.
///
/// `skill` is part of the registry rather than something the frontend bolts
/// on, so everything that runs tools — the main agent and every sub-agent —
/// gets it from one place. Builtin skills (see `skills::builtin_skills`)
/// mean this is never empty, so `skill` is now always present.
pub fn builtin_tools(cwd: &Path) -> Vec<Arc<dyn Tool>> {
    builtin_tools_with_skills_and_web_fetch(
        discover_skills(cwd),
        WebFetchTool::new(trusted_read_hosts(Vec::new())),
        Arc::new(ShellFilters::load(cwd).0),
    )
}

/// Built-in toolset with a startup-configured, tool-scoped set of public read
/// hosts. Empty keeps every web fetch on the ordinary Auto Mode path.
pub fn builtin_tools_with_trusted_read_hosts(
    cwd: &Path,
    trusted_read_hosts: TrustedReadHosts,
) -> Vec<Arc<dyn Tool>> {
    builtin_tools_with_skills_and_web_fetch(
        discover_skills(cwd),
        WebFetchTool::new(trusted_read_hosts),
        Arc::new(ShellFilters::load(cwd).0),
    )
}

pub fn builtin_tools_with_web_fetch(cwd: &Path, web_fetch: WebFetchTool) -> Vec<Arc<dyn Tool>> {
    builtin_tools_with_skills_and_web_fetch(
        discover_skills(cwd),
        web_fetch,
        Arc::new(ShellFilters::load(cwd).0),
    )
}

/// Same toolset, but with skill discovery already done by the caller. Lets a
/// frontend that also needs the `Vec<Skill>` for its own `/name` fallback and
/// completion (see `render_skill`) discover the filesystem once and hand the
/// same result to both places, instead of scanning `.tcode/skills` twice.
pub fn builtin_tools_with_skills(skills: Vec<Skill>) -> Vec<Arc<dyn Tool>> {
    builtin_tools_with_skills_and_web_fetch(
        skills,
        WebFetchTool::new(trusted_read_hosts(Vec::new())),
        Arc::new(ShellFilters::builtin().0),
    )
}

/// Like [`builtin_tools_with_skills`], with a shared, startup-configured set of
/// hosts that `web_fetch` may directly allow in Auto Mode.
pub fn builtin_tools_with_skills_and_trusted_read_hosts(
    skills: Vec<Skill>,
    trusted_read_hosts: TrustedReadHosts,
) -> Vec<Arc<dyn Tool>> {
    builtin_tools_with_skills_and_web_fetch(
        skills,
        WebFetchTool::new(trusted_read_hosts),
        Arc::new(ShellFilters::builtin().0),
    )
}

/// Build a toolset around a fully configured `web_fetch` instance. Its
/// summarizer dependency stays owned by that tool rather than in `ToolCtx`.
pub fn builtin_tools_with_skills_and_web_fetch(
    skills: Vec<Skill>,
    web_fetch: WebFetchTool,
    filters: Arc<ShellFilters>,
) -> Vec<Arc<dyn Tool>> {
    let mut tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(fs::ReadTool),
        Arc::new(fs::WriteTool),
        Arc::new(fs::AppendTool),
        Arc::new(fs::EditTool),
        Arc::new(search::GrepTool),
        Arc::new(search::GlobTool),
        Arc::new(web_fetch),
        Arc::new(web::WebSearchTool),
        Arc::new(shell::KillTaskTool),
    ];
    let primary_shell = primary_shell_kind();
    tools.push(Arc::new(shell::ShellTool::with_filters(
        primary_shell,
        filters.clone(),
    )));
    if cfg!(windows) && shell::bash_available() {
        tools.push(Arc::new(shell::ShellTool::with_filters(
            ShellKind::Bash,
            filters,
        )));
    }
    // The monitor speaks the platform's primary shell, like `shell`.
    tools.push(Arc::new(monitor::MonitorTool::new(primary_shell)));
    if let Some(skill_tool) = SkillTool::new(skills) {
        tools.push(Arc::new(skill_tool));
    }
    tools
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Locating/content tools return precise, self-paginating output and must
    /// bypass the blob gate; command/fetch tools keep it (unpredictable size).
    #[test]
    fn output_gating_is_scoped_to_command_tools() {
        let tools = builtin_tools(&std::env::temp_dir());
        let gates: std::collections::HashMap<&str, bool> =
            tools.iter().map(|t| (t.name(), t.gates_output())).collect();
        for tool in ["read", "grep", "glob", "web_search"] {
            assert!(!gates[tool], "{tool} must not blob-gate its output");
        }
        // web_fetch is always present; the shell tool name is selected once
        // from the platform helper instead of duplicated in consumers.
        assert!(gates["web_fetch"], "web_fetch must keep the blob gate");
        assert_eq!(
            gates.get(primary_shell_tool_name()),
            Some(&true),
            "the primary shell tool must keep the blob gate"
        );
    }

    /// A malformed schema is not a degraded tool — the provider rejects the
    /// whole request, so one bad tool takes the session down before the first
    /// token. `null` is the way to write one by accident (a recursive schema
    /// builder bottoming out), and no schema position accepts it.
    #[test]
    fn every_tool_schema_is_a_legal_json_schema() {
        fn no_nulls(value: &serde_json::Value, path: &str, tool: &str) {
            match value {
                serde_json::Value::Null => {
                    panic!("{tool}'s schema has a null at {path}; JSON Schema has no null nodes")
                }
                serde_json::Value::Object(map) => {
                    for (key, child) in map {
                        no_nulls(child, &format!("{path}.{key}"), tool);
                    }
                }
                serde_json::Value::Array(items) => {
                    for (index, child) in items.iter().enumerate() {
                        no_nulls(child, &format!("{path}[{index}]"), tool);
                    }
                }
                _ => {}
            }
        }

        // Every tool the harness can hand a model, including the main-agent
        // additions that are not part of `builtin_tools`.
        let mut tools = builtin_tools(&std::env::temp_dir());
        tools.push(Arc::new(crate::ProgressTool));
        tools.push(Arc::new(crate::AskUserTool));
        tools.push(Arc::new(crate::AddNoteTool));
        // Registered by the desktop app rather than by `builtin_tools`, but the
        // provider rejects a malformed schema wherever it came from.
        tools.push(Arc::new(crate::ShowTool));
        for tool in &tools {
            let schema = tool.input_schema();
            assert_eq!(
                schema["type"],
                "object",
                "{}'s schema must be an object",
                tool.name()
            );
            no_nulls(&schema, "", tool.name());
        }
    }
}
