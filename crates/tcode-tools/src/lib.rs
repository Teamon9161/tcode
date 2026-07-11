mod fs_tools;
mod grounding;
mod interaction;
mod mcp;
mod output;
mod search;
mod shell;
mod skills;
mod task;
mod web;

pub use grounding::project_map;
pub use interaction::{AddNoteTool, AskUserTool, UpdatePlanTool};
pub use mcp::connect_mcp_servers;
pub use shell::ShellKind;
pub use skills::SkillTool;
pub use task::TaskTool;

use std::sync::Arc;

use tcode_core::Tool;

/// Built-in toolset. On Windows PowerShell is the primary shell and a
/// `bash` tool appears when Git Bash is on PATH; on Unix there is bash.
pub fn builtin_tools() -> Vec<Arc<dyn Tool>> {
    let mut tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(fs_tools::ReadTool),
        Arc::new(fs_tools::WriteTool),
        Arc::new(fs_tools::EditTool),
        Arc::new(search::GrepTool),
        Arc::new(search::GlobTool),
        Arc::new(output::ReadOutputTool),
        Arc::new(web::WebFetchTool),
        Arc::new(web::WebSearchTool),
        Arc::new(shell::KillTaskTool),
    ];
    if cfg!(windows) {
        tools.push(Arc::new(shell::ShellTool::new(ShellKind::PowerShell)));
        if shell::bash_available() {
            tools.push(Arc::new(shell::ShellTool::new(ShellKind::Bash)));
        }
    } else {
        tools.push(Arc::new(shell::ShellTool::new(ShellKind::Bash)));
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
        let tools = builtin_tools();
        let gates: std::collections::HashMap<&str, bool> =
            tools.iter().map(|t| (t.name(), t.gates_output())).collect();
        for tool in ["read", "grep", "glob", "read_output", "web_search"] {
            assert!(!gates[tool], "{tool} must not blob-gate its output");
        }
        // web_fetch is always present; the shell tool is named per platform.
        assert!(gates["web_fetch"], "web_fetch must keep the blob gate");
        let shell = gates.get("shell").or_else(|| gates.get("bash"));
        assert_eq!(shell, Some(&true), "the shell tool must keep the blob gate");
    }
}
