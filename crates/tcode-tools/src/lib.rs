mod fs_tools;
mod grounding;
mod output;
mod search;
mod shell;
mod task;

pub use grounding::project_map;
pub use shell::ShellKind;
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
