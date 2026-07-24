//! From a loaded `Config` to a live `Arc<Agent>`.
//!
//! [`agent::build_agent`](crate::agent) is the last step of that; this is
//! everything leading up to it — discovering and validating agent definitions,
//! loading shell filters, connecting MCP servers, resolving sub-agent pins,
//! finding skills. None of it differs between frontends, and all of it has to
//! happen before the first turn, so a frontend that skipped a step would ship a
//! subtly weaker agent rather than an obviously broken one.
//!
//! What stays with each frontend is what it alone knows: CLI parsing, the
//! first-run wizard, and how to show the `warnings` this returns.

use std::path::PathBuf;
use std::sync::Arc;

use tcode_core::config::{Config, ModelState, Selection};
use tcode_core::{Agent, AgentModels, ModelCell, PermissionMode, PermissionRules};
use tcode_tools::{AgentDef, AgentRegistry, ShellFilters, Skill};

use crate::agent::{build_agent, AgentBuild};

/// The interactive system prompt. It lives here rather than in a binary
/// because every interactive frontend serves the same agent — a desktop
/// session and a terminal session must not be talking to different personas.
pub const INTERACTIVE_AGENT_SYSTEM: &str = include_str!("../prompts/interactive-agent-system.md");

/// What a frontend knows before the agent exists.
pub struct BootSpec<'a> {
    pub cwd: PathBuf,
    /// Mutated in place: `trusted_read_hosts` is taken out of it and agent
    /// definition hints are applied back into it, so the caller's copy is the
    /// one the rest of the session reads.
    pub config: &'a mut Config,
    pub selection: Selection,
    pub model_cell: ModelCell,
    /// `--agent <name>`: run *as* that definition. Unknown names are an error
    /// rather than a warning — the user asked for a specific agent and would
    /// otherwise silently get the ordinary one.
    pub agent: Option<String>,
}

/// The assembled agent plus the pieces frontends need alongside it.
pub struct Booted {
    pub agent: Arc<Agent>,
    /// Live sub-agent pins, shared with `/agents` and the `agent` tool.
    pub pinned: AgentModels,
    pub agent_defs: Arc<AgentRegistry>,
    /// The `/cd`-aware filter chain, registered on each session's scope.
    pub shell_filters: Arc<ShellFilters>,
    /// Discovered once and shared, so completion menus never list a different
    /// set of skills than the `skill` tool actually has.
    pub skills: Vec<Skill>,
    /// The resolved `--agent` definition, if one was named.
    pub cli_agent: Option<AgentDef>,
    /// Everything that degraded quietly: a bad agent definition, an
    /// unreachable MCP server, an invalid pin. Never printed here — see the
    /// crate's AGENTS.md.
    pub warnings: Vec<String>,
}

/// Assemble the agent. `async` because MCP servers are connected here.
pub async fn boot(spec: BootSpec<'_>) -> anyhow::Result<Booted> {
    let BootSpec {
        cwd,
        config,
        selection,
        model_cell,
        agent: named_agent,
    } = spec;
    let mut warnings = Vec::new();

    // Builtin agent kinds plus user-defined `.tcode/agents/*.md` share one
    // registry. Validate their capability policies only after MCP connections
    // have supplied the exact delegated inventory.
    let (mut agent_defs, agent_warnings) = AgentRegistry::discover(&cwd);
    warnings.extend(agent_warnings);
    // Shell output filters: built-ins plus the user's and the project's
    // `filters.toml`. `[limits] shell_output_filters` is read from the user's
    // own configuration only, so a checked-out repository cannot switch
    // filtering back on for someone who turned it off.
    let shell_filters = Arc::new(if config.limits.shell_output_filters {
        let (filters, filter_warnings) = ShellFilters::load(&cwd);
        warnings.extend(filter_warnings);
        filters
    } else {
        ShellFilters::disabled()
    });
    let classifier_policy = tcode_core::classifier_policy(&config.auto_mode);
    let classifier_config = config.auto_mode.classifier_config();
    let trusted_read_hosts =
        tcode_tools::trusted_read_hosts(std::mem::take(&mut config.auto_mode.trusted_read_hosts));
    // MCP servers from config; a broken server warns instead of blocking.
    let mcp_tools = if config.mcp_servers.is_empty() {
        Vec::new()
    } else {
        let (mcp_tools, mcp_warnings) =
            tcode_tools::connect_mcp_servers(&config.mcp_servers, &cwd).await;
        warnings.extend(mcp_warnings);
        mcp_tools
    };
    let definition_validator = tcode_tools::AgentTool::new(
        model_cell.clone(),
        config.watchdog.clone(),
        config.limits.tool_output_tokens,
        cwd.clone(),
    )
    .with_trusted_read_hosts(trusted_read_hosts.clone())
    .with_extension_tools(mcp_tools.clone());
    warnings.extend(definition_validator.validate_definitions(&mut agent_defs, &cwd));
    crate::build::apply_agent_def_hints(config, &agent_defs);
    let agent_defs = Arc::new(agent_defs);

    // `--agent <name>`: this process runs *as* that definition. Resolved
    // before anything enters the prompt prefix; everything it changes
    // (system prompt, toolset, model, max_steps) is fixed at startup.
    let cli_agent = match named_agent.as_deref() {
        Some(name) => {
            let Some(def) = agent_defs.get(name) else {
                anyhow::bail!(
                    "unknown agent '{name}'; available: {}",
                    agent_defs.names_for(None).join(", ")
                );
            };
            Some(def.clone())
        }
        None => None,
    };
    // Live sub-agent pins, shared by the `agent` tool and `/agents`.
    let (pinned, pin_warnings) = crate::build::agent_models(config, &selection);
    warnings.extend(pin_warnings);
    if let Some(def) = &cli_agent {
        // A pinned model for the named agent becomes the session model
        // (process-local; never persisted to [tcode_state] in the selected config).
        if let Some(model) = pinned.get(&def.name) {
            model_cell.swap(model);
        }
    }

    let system = match &cli_agent {
        Some(def) => def.system.clone(),
        None => INTERACTIVE_AGENT_SYSTEM.to_string(),
    };
    // Discovered once and handed to both the tool and the frontends (TUI
    // completion/`/name` fallback, plain REPL fallback) so they never see a
    // different skill list than the `skill` tool the model calls.
    let skills = tcode_tools::discover_skills(&cwd);
    let agent = build_agent(AgentBuild {
        cwd,
        config,
        selection,
        model_cell,
        pinned: pinned.clone(),
        agent_defs: agent_defs.clone(),
        cli_agent: cli_agent.clone(),
        system,
        skills: skills.clone(),
        shell_filters: shell_filters.clone(),
        trusted_read_hosts,
        classifier_policy,
        classifier_config,
        mcp_tools,
    });

    Ok(Booted {
        agent,
        pinned,
        agent_defs,
        shell_filters,
        skills,
        cli_agent,
        warnings,
    })
}

/// The startup permission mode, with the precedence every frontend uses: an
/// explicit request beats what the user last switched to (`[tcode_state]`),
/// which beats the configured default.
pub fn startup_mode(
    requested: Option<&str>,
    state: &ModelState,
    config: &Config,
) -> anyhow::Result<PermissionMode> {
    Ok(match requested {
        Some("plan") => PermissionMode::Plan,
        Some("accept-edits") => PermissionMode::AcceptEdits,
        Some("auto") => PermissionMode::Auto,
        Some("unsafe") => PermissionMode::Unsafe,
        Some("default") => PermissionMode::Default,
        Some(other) => anyhow::bail!("unknown mode '{other}'"),
        None => state.mode.unwrap_or(config.permissions.mode),
    })
}

/// The configured permission rules, as a session takes them.
pub fn startup_rules(config: &Config) -> PermissionRules {
    PermissionRules {
        allow: config.permissions.allow.clone(),
        ask: config.permissions.ask.clone(),
        deny: config.permissions.deny.clone(),
    }
}
