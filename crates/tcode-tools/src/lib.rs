mod fs_tools;
mod grounding;
mod interaction;
mod mcp;
mod monitor;
mod plan;
mod search;
mod shell;
mod skills;
mod task;
mod view_image;
mod web;

#[cfg(test)]
mod view_image_tests;

pub use grounding::{
    environment_snapshot, project_map, project_map_with_scratch, startup_context_with_scratch,
};
pub use interaction::{AddNoteTool, AskUserTool, UpdateProgressTool};
pub use mcp::connect_mcp_servers;
pub use shell::ShellKind;
pub use skills::{
    discover_skills, parse_skill_echo, render_skill, wrap_skill_echo, Skill, SkillEcho,
    SkillSource, SkillTool,
};
pub use task::{TaskTool, MODEL_ROLES, TASK_AGENT_KINDS};
pub use view_image::ViewImageTool;

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
    builtin_tools_with_skills(discover_skills(cwd))
}

/// Same toolset, but with skill discovery already done by the caller. Lets a
/// frontend that also needs the `Vec<Skill>` for its own `/name` fallback and
/// completion (see `render_skill`) discover the filesystem once and hand the
/// same result to both places, instead of scanning `.tcode/skills` twice.
pub fn builtin_tools_with_skills(skills: Vec<Skill>) -> Vec<Arc<dyn Tool>> {
    let mut tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(fs_tools::ReadTool),
        Arc::new(fs_tools::WriteTool),
        Arc::new(fs_tools::AppendTool),
        Arc::new(fs_tools::EditTool),
        Arc::new(search::GrepTool),
        Arc::new(search::GlobTool),
        Arc::new(web::WebFetchTool),
        Arc::new(web::WebSearchTool),
        Arc::new(shell::KillTaskTool),
        Arc::new(plan::ExitPlanTool),
    ];
    let primary_shell = if cfg!(windows) {
        ShellKind::PowerShell
    } else {
        ShellKind::Bash
    };
    tools.push(Arc::new(shell::ShellTool::new(primary_shell)));
    if cfg!(windows) && shell::bash_available() {
        tools.push(Arc::new(shell::ShellTool::new(ShellKind::Bash)));
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
        // web_fetch is always present; the shell tool is named per platform.
        assert!(gates["web_fetch"], "web_fetch must keep the blob gate");
        let shell = gates.get("shell").or_else(|| gates.get("bash"));
        assert_eq!(shell, Some(&true), "the shell tool must keep the blob gate");
    }
}
