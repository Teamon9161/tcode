//! Assemble the live `Arc<Agent>` from resolved configuration.
//!
//! This is the toolset + `agent` tool + safety classifier + `Agent` wiring the
//! composition root used to inline. The desktop app's backend builds its agent
//! through here exactly as the CLI binary does; only the caller-specific parts
//! (CLI parsing, menu construction, warning output) stay in each frontend.

use std::path::PathBuf;
use std::sync::Arc;

use tcode_core::config::{AutoClassifierConfig, Config, Selection};
use tcode_core::{Agent, AgentModels, ModelCell, ProviderSafetyClassifier, SafetyClassifier, Tool};
use tcode_tools::{
    keeps_tool, AddNoteTool, AgentDef, AgentRegistry, AgentTool, AskUserTool, FetchSummarizer,
    ShellFilters, Skill, TrustedReadHosts, UpdateProgressTool, ViewImageTool, WebFetchTool,
};

/// Resolved inputs for [`build_agent`]. The caller resolves models, agent
/// definitions, skills and MCP tools first (those steps differ per frontend
/// and emit their own warnings); this struct is the point where they converge
/// into one agent.
pub struct AgentBuild<'a> {
    pub cwd: PathBuf,
    pub config: &'a Config,
    /// The main model selection, cloned into the spawn-time model resolver.
    pub selection: Selection,
    pub model_cell: ModelCell,
    /// Live sub-agent pins shared by the `agent` tool and the classifier.
    pub pinned: AgentModels,
    pub agent_defs: Arc<AgentRegistry>,
    /// `--agent <name>`: this agent runs *as* that definition (shapes toolset,
    /// system prompt and `max_steps`). `None` is the ordinary interactive run.
    pub cli_agent: Option<AgentDef>,
    pub system: String,
    pub skills: Vec<Skill>,
    pub shell_filters: Arc<ShellFilters>,
    pub trusted_read_hosts: TrustedReadHosts,
    pub classifier_policy: String,
    pub classifier_config: AutoClassifierConfig,
    /// Pre-connected MCP tools, appended to the toolset and handed to the
    /// `agent` tool so sub-agents inherit them.
    pub mcp_tools: Vec<Arc<dyn Tool>>,
}

/// Build the interactive agent. Faithful extraction of the composition root's
/// inline assembly — behavior is unchanged.
pub fn build_agent(build: AgentBuild<'_>) -> Arc<Agent> {
    let AgentBuild {
        cwd,
        config,
        selection,
        model_cell,
        pinned,
        agent_defs,
        cli_agent,
        system,
        skills,
        shell_filters,
        trusted_read_hosts,
        classifier_policy,
        classifier_config,
        mcp_tools,
    } = build;

    let mut tools = tcode_tools::builtin_tools_with_skills_and_web_fetch(
        skills,
        WebFetchTool::new(trusted_read_hosts.clone())
            .with_summarizer(FetchSummarizer::new(model_cell.clone(), pinned.clone())),
        shell_filters.clone(),
    );
    tools.push(Arc::new(ViewImageTool::new(
        model_cell.clone(),
        pinned.clone(),
    )));
    tools.push(Arc::new(UpdateProgressTool));
    tools.push(Arc::new(AskUserTool));
    tools.push(Arc::new(AddNoteTool));
    tools.extend(mcp_tools.iter().cloned());
    let agent_tool = AgentTool::new(
        model_cell.clone(),
        config.watchdog.clone(),
        config.limits.tool_output_tokens,
        cwd.clone(),
    )
    .with_agent_models(pinned.clone())
    .with_agent_defs(agent_defs.clone())
    .with_auto_policy(classifier_policy.clone())
    .with_auto_classifier_config(classifier_config)
    .with_auto_compact(
        config.limits.auto_compact,
        config.limits.auto_compact_percent,
    )
    .with_trusted_read_hosts(trusted_read_hosts.clone())
    .with_shell_filters(shell_filters.clone())
    .with_model_resolver({
        // Lets the model honor "delegate this on <model>" at spawn time. The
        // catalogue is a startup snapshot: a `/provider` reload that adds a
        // profile mid-session is not reflected here, which is acceptable —
        // profiles are set up once, and named ids pass through verbatim anyway.
        let config = config.clone();
        let parent = selection.clone();
        Arc::new(move |model: Option<&str>, effort: Option<&str>| {
            let sel = config
                .resolve_model_override(model, effort, &parent)
                .map_err(|e| e.to_string())?;
            let profile = config
                .profiles
                .get(&sel.profile)
                .ok_or_else(|| format!("profile '{}' is not configured", sel.profile))?;
            tcode_providers::build_active(profile, &sel, &config.watchdog)
                .map_err(|e| e.to_string())
        })
    })
    .with_extension_tools(mcp_tools);
    // A named-agent run shapes the toolset last: allowlist filtering over
    // everything assembled above, then the agent tool — which is granted by
    // the definition's spawn policy alone, outside the allowlist tiers.
    match &cli_agent {
        Some(def) => {
            tools.retain(|tool| keeps_tool(def, tool.as_ref()));
            if !agent_defs.spawn_list(def).is_empty() {
                tools.push(Arc::new(agent_tool.scoped_to(def)));
            }
        }
        None => tools.push(Arc::new(agent_tool)),
    }
    let safety_classifier: Arc<dyn SafetyClassifier> = Arc::new(
        ProviderSafetyClassifier::new(model_cell.clone(), pinned.clone())
            .with_config(classifier_config),
    );
    Arc::new(Agent {
        model: model_cell.clone(),
        models: pinned.clone(),
        tools,
        system,
        watchdog: config.watchdog.clone(),
        hooks: tcode_core::Hooks::new(config.hooks.clone()),
        safety_classifier: Some(safety_classifier),
        auto_policy: classifier_policy,
        max_steps: cli_agent
            .as_ref()
            .and_then(|def| def.max_steps)
            .unwrap_or(config.limits.max_steps_per_turn),
        auto_compact: config.limits.auto_compact,
        auto_compact_percent: config.limits.auto_compact_percent,
    })
}
