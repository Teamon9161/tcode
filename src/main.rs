mod approver;
mod printer;
mod update;

use std::io::{IsTerminal, Write as _};
use std::sync::Arc;

use anyhow::Context;
use clap::{Parser, Subcommand};
use tokio_util::sync::CancellationToken;

use tcode_core::commands::{CommandCtx, CommandEffect, CommandRegistry, MessageKind};
use tcode_core::config::{AgentConfig, Config, ConfigError, ModelState, Selection};
use tcode_core::{
    ActiveModel, Agent, AgentError, AgentModels, AgentRole, ContentBlock, ModelCell,
    PermissionRules, ProviderSafetyClassifier, SafetyClassifier, Session, ToolCtx,
};

const CYAN: &str = "\x1b[36m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

const INTERACTIVE_AGENT_SYSTEM: &str = include_str!("../prompts/interactive-agent-system.md");
const AUTO_CLASSIFIER_POLICY: &str = include_str!("../prompts/auto-classifier-policy.md");

/// Fixed classifier policy with optional user-owned global refinements. The
/// classifier never receives project-local config, because a repository must
/// not be able to relax the safety gate that protects it.
fn auto_policy(config: &Config) -> String {
    let mut policy = format!("{AUTO_CLASSIFIER_POLICY}\n");
    let append = |policy: &mut String, heading: &str, rules: &[String]| {
        if rules.is_empty() {
            return;
        }
        policy.push_str(heading);
        policy.push('\n');
        for rule in rules {
            policy.push_str("- ");
            policy.push_str(rule);
            policy.push('\n');
        }
        policy.push('\n');
    };
    append(
        &mut policy,
        "Hard deny rules (never override):",
        &config.auto_mode.hard_deny,
    );
    append(
        &mut policy,
        "Soft deny rules (specific user intent may override):",
        &config.auto_mode.soft_deny,
    );
    append(
        &mut policy,
        "Allowed exceptions to soft denies:",
        &config.auto_mode.allow,
    );
    policy
}

const CONFIG_HEADER: &str = "\
# tcode global configuration — created by the setup wizard.
# Add profiles/models freely; the active choice lives in state.toml
# (written by /model). Keys: prefer api_key_env over inline api_key.

";

/// Build the provider for one sub-agent pin.
fn build_agent_model(
    config: &Config,
    kind: &str,
    parent: &Selection,
) -> Option<Result<ActiveModel, ConfigError>> {
    let resolved = config.agent_selection(kind, parent)?;
    Some(resolved.and_then(|selection| {
        let profile = config
            .profiles
            .get(&selection.profile)
            .expect("agent_selection validated the profile");
        tcode_providers::build_active(profile, &selection, &config.watchdog)
    }))
}

/// Resolve every `[agents.<kind>]` / `/agents` pin into the live registry the
/// tools read. `fetch` is opt-in: its explicit `enabled = true` assignment
/// inherits the main model, while an absent assignment leaves it off.
fn agent_models(config: &Config, parent: &Selection) -> AgentModels {
    let pinned = AgentModels::default();
    for (kind, assignment) in &config.agents {
        if assignment.enabled == Some(false) {
            continue;
        }
        match build_agent_model(config, kind, parent) {
            Some(Ok(model)) => pinned.pin(kind, model),
            Some(Err(e)) => eprintln!("{DIM}warning: [agents.{kind}] ignored: {e}{RESET}"),
            None if AgentRole::from_key(kind)
                .is_some_and(|role| role.allows_off() && assignment.enabled == Some(true)) =>
            {
                pinned.pin_inherit(kind)
            }
            None => {}
        }
    }
    pinned
}

/// The `/agents` menu: the pinnable kinds, what each runs on now, and the
/// action that applies a pick — hot-swap the shared registry, then persist to
/// state.toml (never to the hand-written config.toml).
fn build_agent_menu(
    config: &Config,
    menu: &tcode_tui::ModelMenu,
    pinned: AgentModels,
) -> tcode_tui::AgentMenu {
    let roles: Vec<tcode_tui::AgentRole> = AgentRole::ALL
        .iter()
        .map(|role| tcode_tui::AgentRole {
            key: role.key().to_string(),
            label: role.label().to_string(),
            allows_off: role.allows_off(),
        })
        .collect();
    let watchdog = config.watchdog.clone();
    let profiles = config.profiles.clone();
    let options: Vec<(String, tcode_core::config::ModelDef)> = menu
        .options
        .iter()
        .map(|option| (option.profile.clone(), option.def.clone()))
        .collect();
    let pins = roles
        .iter()
        .map(|role| {
            if let Some(model) = pinned.get(&role.key) {
                let Some(option) = menu
                    .options
                    .iter()
                    .position(|opt| opt.def.name == model.provider.model())
                else {
                    return tcode_tui::AgentModelChoice::Inherit;
                };
                tcode_tui::AgentModelChoice::Model {
                    option,
                    effort: model.effort.clone(),
                }
            } else if pinned.inherits(&role.key) {
                tcode_tui::AgentModelChoice::Inherit
            } else if role.allows_off {
                tcode_tui::AgentModelChoice::Off
            } else {
                tcode_tui::AgentModelChoice::Inherit
            }
        })
        .collect();

    let pin: tcode_tui::PinFn = Box::new(move |kind, choice| {
        let role = AgentRole::from_key(kind).expect("menu only exposes registered roles");
        match choice {
            tcode_tui::AgentModelChoice::Off => {
                pinned.unpin(kind);
                persist_agent_pin(role, None, false);
                Ok("off".to_string())
            }
            tcode_tui::AgentModelChoice::Inherit => {
                pinned.pin_inherit(kind);
                persist_agent_pin(role, None, true);
                Ok("inherit (main model)".to_string())
            }
            tcode_tui::AgentModelChoice::Model { option, effort } => {
                let (profile_name, model) = options
                    .get(option)
                    .ok_or_else(|| "selected model disappeared".to_string())?;
                let profile = profiles
                    .get(profile_name)
                    .ok_or_else(|| format!("unknown profile '{profile_name}'"))?;
                let selection = Selection {
                    profile: profile_name.clone(),
                    model: model.clone(),
                    effort,
                };
                let active = tcode_providers::build_active(profile, &selection, &watchdog)
                    .map_err(|e| e.to_string())?;
                let label = active.describe();
                pinned.pin(kind, active);
                persist_agent_pin(role, Some(&selection), true);
                Ok(label)
            }
        }
    });
    tcode_tui::AgentMenu { roles, pins, pin }
}

/// State entries override `[agents.*]`: an explicit `enabled` lets opt-in
/// roles preserve the distinction between "off" and "inherit main model".
fn persist_agent_pin(role: AgentRole, selection: Option<&Selection>, enabled: bool) {
    let enabled = role.allows_off().then_some(enabled);
    ModelState::update(|state| {
        state.agents.insert(
            role.key().to_string(),
            match selection {
                Some(s) => AgentConfig {
                    profile: Some(s.profile.clone()),
                    model: Some(s.model.name.clone()),
                    effort: s.effort.clone(),
                    enabled,
                },
                None => AgentConfig {
                    enabled,
                    ..AgentConfig::default()
                },
            },
        );
    });
}

/// Flatten every profile's models into the /model menu, wiring the
/// switch action (rebuild provider + persist choice).
fn build_menu(
    config: &Config,
    selection: &Selection,
    _model_cell: ModelCell,
) -> tcode_tui::ModelMenu {
    let mut options = Vec::new();
    let mut current = 0;
    for (pname, profile) in &config.profiles {
        // The built-in catalog always contributes every provider; hide the
        // ones the user has no credentials for so the picker stays short.
        // The active profile is always shown so `current` stays valid.
        if !tcode_providers::profile_is_usable(pname, profile) && pname != &selection.profile {
            continue;
        }
        for def in profile.model_defs() {
            if pname == &selection.profile && def.name == selection.model.name {
                current = options.len();
            }
            options.push(tcode_tui::ModelOption {
                profile: pname.clone(),
                def,
            });
        }
    }
    let cfg = config.clone();
    let watchdog = config.watchdog.clone();
    let switch: tcode_tui::SwitchFn = Box::new(move |opt, effort| {
        let profile = cfg
            .profiles
            .get(&opt.profile)
            .ok_or_else(|| format!("profile '{}' not found", opt.profile))?;
        let sel = Selection {
            profile: opt.profile.clone(),
            model: opt.def.clone(),
            effort: effort.map(String::from),
        };
        let active =
            tcode_providers::build_active(profile, &sel, &watchdog).map_err(|e| e.to_string())?;
        // Read-modify-write: the state file also holds the agent pins and the
        // dogfood switch, which a whole-struct save would silently drop.
        ModelState::update(|state| {
            state.profile = Some(opt.profile.clone());
            state.model = Some(opt.def.name.clone());
            state.effort = effort.map(String::from);
        });
        Ok(active)
    });
    tcode_tui::ModelMenu {
        options,
        current,
        switch,
    }
}

/// Plain-REPL `/model`: bare lists options, `/model <n|name> [effort]`
/// switches.
fn run_model_command(args: &str, menu: &tcode_tui::ModelMenu, cell: &ModelCell) {
    if args.is_empty() {
        let active = cell.snapshot();
        for (i, opt) in menu.options.iter().enumerate() {
            let mark = if opt.def.name == active.provider.model() {
                "●"
            } else {
                " "
            };
            let efforts = if opt.def.efforts.is_empty() {
                String::new()
            } else {
                format!("  [{}]", opt.def.efforts.join("/"))
            };
            println!(
                "{DIM} {mark} {i}: {} · {}{efforts}{RESET}",
                opt.profile, opt.def.name
            );
        }
        println!("{DIM}usage: /model <number|name> [effort]{RESET}");
        return;
    }
    let mut parts = args.split_whitespace();
    let which = parts.next().unwrap_or_default();
    let effort = parts.next();
    let found = which
        .parse::<usize>()
        .ok()
        .and_then(|i| menu.options.get(i))
        .or_else(|| menu.options.iter().find(|o| o.def.name == which));
    let Some(opt) = found else {
        println!("{DIM}unknown model '{which}' — /model lists options{RESET}");
        return;
    };
    match (menu.switch)(opt, effort) {
        Ok(active) => {
            println!(
                "{DIM}model → {} · {}{RESET}",
                active.provider.name(),
                active.describe()
            );
            cell.swap(active);
        }
        Err(e) => println!("{DIM}cannot switch model: {e}{RESET}"),
    }
}

/// Plain-REPL `/agents`: bare lists each role; `/agents <role>
/// <off|inherit|n|name> [effort]` assigns it. The TUI uses the same modes.
fn run_agents_command(args: &str, menu: &tcode_tui::ModelMenu, agents: &mut tcode_tui::AgentMenu) {
    if args.is_empty() {
        for (i, role) in agents.roles.iter().enumerate() {
            println!("{DIM} {}: {}{RESET}", role.label, agents.describe(i, menu));
        }
        println!("{DIM}usage: /agents <role> <off|inherit|number|name> [effort]{RESET}");
        return;
    }
    let mut parts = args.split_whitespace();
    let (Some(role_name), Some(which)) = (parts.next(), parts.next()) else {
        println!("{DIM}usage: /agents <role> <off|inherit|number|name> [effort]{RESET}");
        return;
    };
    let Some(slot) = agents
        .roles
        .iter()
        .position(|role| role.key == role_name || role.label == role_name)
    else {
        println!(
            "{DIM}unknown role '{role_name}' — known: {}{RESET}",
            agents
                .roles
                .iter()
                .map(|role| role.label.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        return;
    };
    let role = &agents.roles[slot];
    let effort = parts.next();
    let choice = match which {
        "off" if role.allows_off => tcode_tui::AgentModelChoice::Off,
        "off" => {
            println!("{DIM}{} cannot be turned off{RESET}", role.label);
            return;
        }
        "inherit" => tcode_tui::AgentModelChoice::Inherit,
        _ => {
            let found = which
                .parse::<usize>()
                .ok()
                .and_then(|i| menu.options.get(i).map(|_| i))
                .or_else(|| {
                    menu.options
                        .iter()
                        .position(|option| option.def.name == which)
                });
            let Some(option) = found else {
                println!("{DIM}unknown model '{which}' — /model lists options{RESET}");
                return;
            };
            tcode_tui::AgentModelChoice::Model {
                option,
                effort: effort.map(String::from),
            }
        }
    };
    let key = role.key.clone();
    match (agents.pin)(&key, choice.clone()) {
        Ok(label) => {
            agents.pins[slot] = choice;
            println!("{DIM}{} → {label}{RESET}", role.label);
        }
        Err(e) => println!("{DIM}cannot configure {}: {e}{RESET}", role.label),
    }
}

#[derive(Parser)]
#[command(name = "tcode", version, about = "tcode — a terminal agent harness")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Config profile to use (from ~/.tcode/config.toml)
    #[arg(long)]
    profile: Option<String>,
    /// Override the profile's model
    #[arg(long)]
    model: Option<String>,
    /// One-shot prompt: run the full agent loop, print, exit
    #[arg(short = 'p', long)]
    prompt: Option<String>,
    /// Start in a specific permission mode (plan/default/accept-edits/auto/unsafe)
    #[arg(long)]
    mode: Option<String>,
    /// Continue the most recent session in this project
    #[arg(short = 'c', long = "continue")]
    r#continue: bool,
    /// Resume a session by id (prefix is enough)
    #[arg(long)]
    resume: Option<String>,
    /// Run as a named agent definition (`.tcode/agents/<name>.md`): its
    /// system prompt, toolset, and model pin replace the interactive defaults
    #[arg(long)]
    agent: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Download and install the latest GitHub Release for this platform
    Update,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if matches!(cli.command, Some(Command::Update)) {
        return update::run().await;
    }
    let cwd = std::env::current_dir()
        .context("cannot determine working directory")?
        .canonicalize()
        .context("cannot canonicalize working directory")?;
    let interactive = std::io::stdout().is_terminal() && std::io::stdin().is_terminal();

    // First run: no global config yet. Interactive terminals get the
    // setup wizard; pipes/CI fall back to an env-key-based default.
    if !Config::exists() {
        if interactive && cli.prompt.is_none() {
            match tcode_tui::wizard::run()? {
                Some((config, state)) => {
                    let path = config.write_global(CONFIG_HEADER)?;
                    state.save();
                    println!("{DIM}wrote {}{RESET}\n", path.display());
                }
                None => anyhow::bail!("setup cancelled — no config written"),
            }
        } else {
            tcode_tui::wizard::default_config().write_global(CONFIG_HEADER)?;
        }
    }

    let (mut config, selection, active_model, state) = loop {
        let mut config = Config::load(&cwd)?;
        tcode_providers::hydrate_codex_models(&mut config);
        let state = ModelState::load();
        // `/agents` picks live in state.toml and overlay the hand-written
        // `[agents.*]`, exactly as the saved model choice overlays the
        // configured default.
        config.agents.extend(state.agents.clone());
        let selection = config.select(cli.profile.as_deref(), cli.model.as_deref(), &state)?;
        let profile = config
            .profiles
            .get(&selection.profile)
            .context("selected profile disappeared")?;
        match tcode_providers::build_active(profile, &selection, &config.watchdog) {
            Ok(active) => break (config, selection, active, state),
            Err(ConfigError::MissingApiKey {
                profile: missing_profile,
                ..
            }) if interactive && cli.prompt.is_none() => {
                // Load only global settings: project overlays must never be
                // copied into ~/.tcode/config.toml by the setup wizard.
                let global = Config::load_global()?;
                let Some((updated, state)) =
                    tcode_tui::wizard::reconfigure(global, &missing_profile)?
                else {
                    anyhow::bail!("setup cancelled — no usable provider configured")
                };
                let path = updated.write_global(CONFIG_HEADER)?;
                state.save();
                println!("{DIM}updated {}{RESET}\n", path.display());
            }
            Err(error) => return Err(error.into()),
        }
    };
    let model_cell = ModelCell::new(active_model);

    // Everything /model can switch to, with the swap logic attached.
    let mut menu = build_menu(&config, &selection, model_cell.clone());
    // Builtin task kinds + user-defined agents (`.tcode/agents/*.md`), one
    // registry shared by the task tool. Front-matter model hints become
    // `[agents.<name>]` defaults, so hand-written config and `/agents` picks
    // win through the ordinary resolution below.
    let (agent_defs, agent_warnings) = tcode_tools::AgentRegistry::discover(&cwd);
    for warning in &agent_warnings {
        eprintln!("{DIM}warning: {warning}{RESET}");
    }
    for def in agent_defs.custom() {
        if let Some(hint) = &def.model {
            config
                .agents
                .entry(def.name.clone())
                .or_insert_with(|| AgentConfig {
                    profile: hint.profile.clone(),
                    model: hint.model.clone(),
                    effort: hint.effort.clone(),
                    enabled: None,
                });
        }
    }
    let agent_defs = Arc::new(agent_defs);
    // `--agent <name>`: this process runs *as* that definition. Resolved
    // before anything enters the prompt prefix; everything it changes
    // (system prompt, toolset, model, max_steps) is fixed at startup.
    let cli_agent = match cli.agent.as_deref() {
        Some(name) => {
            if cli.r#continue || cli.resume.is_some() {
                anyhow::bail!(
                    "--agent cannot be combined with --continue/--resume: \
                     a resumed session was recorded under a different system prompt"
                );
            }
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
    // Live sub-agent pins, shared by the `task` tool and `/agents`.
    let pinned = agent_models(&config, &selection);
    if let Some(def) = &cli_agent {
        // A pinned model for the named agent becomes the session model
        // (process-local; never persisted to state.toml).
        if let Some(model) = pinned.get(&def.name) {
            model_cell.swap(model);
        }
    }
    let mut agent_menu = build_agent_menu(&config, &menu, pinned.clone());

    let system = match &cli_agent {
        Some(def) => def.system.clone(),
        None => INTERACTIVE_AGENT_SYSTEM.to_string(),
    };
    let classifier_policy = auto_policy(&config);
    let trusted_read_hosts =
        tcode_tools::trusted_read_hosts(std::mem::take(&mut config.auto_mode.trusted_read_hosts));
    // Discovered once and handed to both the tool and the frontends (TUI
    // completion/`/name` fallback, plain REPL fallback) so they never see a
    // different skill list than the `skill` tool the model calls.
    let skills = tcode_tools::discover_skills(&cwd);
    let mut tools = tcode_tools::builtin_tools_with_skills_and_web_fetch(
        skills.clone(),
        tcode_tools::WebFetchTool::new(trusted_read_hosts.clone()).with_summarizer(
            tcode_tools::FetchSummarizer::new(model_cell.clone(), pinned.clone()),
        ),
    );
    tools.push(Arc::new(tcode_tools::ViewImageTool::new(
        model_cell.clone(),
        pinned.clone(),
    )));
    let task_tool = tcode_tools::TaskTool::new(
        model_cell.clone(),
        config.watchdog.clone(),
        config.limits.tool_output_tokens,
        cwd.clone(),
    )
    .with_agent_models(pinned.clone())
    .with_agent_defs(agent_defs.clone())
    .with_auto_policy(classifier_policy.clone())
    .with_auto_compact(
        config.limits.auto_compact,
        config.limits.auto_compact_percent,
    )
    .with_trusted_read_hosts(trusted_read_hosts.clone());
    tools.push(Arc::new(tcode_tools::UpdateProgressTool));
    tools.push(Arc::new(tcode_tools::AskUserTool));
    tools.push(Arc::new(tcode_tools::AddNoteTool));
    // MCP servers from config; a broken server warns instead of blocking.
    if !config.mcp_servers.is_empty() {
        let (mcp_tools, warnings) =
            tcode_tools::connect_mcp_servers(&config.mcp_servers, &cwd).await;
        for warning in warnings {
            eprintln!("{DIM}warning: {warning}{RESET}");
        }
        tools.extend(mcp_tools);
    }
    // Allowlists may only be checked against the fully assembled toolset
    // (MCP included); definitions with unknown names stay usable elsewhere.
    let tool_names: Vec<&str> = tools.iter().map(|tool| tool.name()).collect();
    for warning in agent_defs.validate_tools(&tool_names) {
        eprintln!("{DIM}warning: {warning}{RESET}");
    }
    // A named-agent run shapes the toolset last: allowlist filtering over
    // everything assembled above, then the task tool — which is granted by
    // the definition's `agents` field alone, outside the allowlist tiers.
    match &cli_agent {
        Some(def) => {
            tools.retain(|tool| tcode_tools::keeps_tool(def, tool.as_ref()));
            if !def.agents.is_empty() {
                tools.push(Arc::new(task_tool.scoped_to(def)));
            }
        }
        None => tools.push(Arc::new(task_tool)),
    }
    let safety_classifier: Arc<dyn SafetyClassifier> = Arc::new(ProviderSafetyClassifier::new(
        model_cell.clone(),
        pinned.clone(),
    ));
    let agent = Arc::new(Agent {
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
    });

    let mode = match cli.mode.as_deref() {
        Some("plan") => tcode_core::PermissionMode::Plan,
        Some("accept-edits") => tcode_core::PermissionMode::AcceptEdits,
        Some("auto") => tcode_core::PermissionMode::Auto,
        Some("unsafe") => tcode_core::PermissionMode::Unsafe,
        Some("default") => tcode_core::PermissionMode::Default,
        Some(other) => anyhow::bail!("unknown mode '{other}'"),
        // Same precedence as the model choice: CLI flag > what the user last
        // switched to (state.toml) > the configured default.
        None => state.mode.unwrap_or(config.permissions.mode),
    };
    let rules = PermissionRules {
        allow: config.permissions.allow.clone(),
        ask: config.permissions.ask.clone(),
        deny: config.permissions.deny.clone(),
    };
    let mut session = Session::new(
        ToolCtx::new(cwd.clone(), config.limits.tool_output_tokens).with_model(model_cell.clone()),
        mode,
        rules,
    );
    session.set_dogfood(state.dogfood);
    if let Some(trust) = state.folder_trust_for(&cwd) {
        session.set_folder_trust(trust);
    }
    // `/suggestions` last, else the config default. Same precedence as the
    // model choice: what the user last chose beats what the file says.
    session.set_suggestions(state.suggestions.unwrap_or(config.ui.suggest_next));

    let opening_context: tcode_tui::OpeningContextFn =
        Arc::new(tcode_tools::startup_context_with_scratch);
    let environment: tcode_tui::EnvironmentFn = Arc::new(tcode_tools::environment_snapshot);

    // Persistence: every ledger mutation is recorded to a JSONL session
    // log; --continue / --resume replay it.
    if let Some(data_dir) = tcode_core::store::project_data_dir(&cwd) {
        // Before this run's log exists, so the empty log we are about to create
        // is not mistaken for one of the abandoned ones it collects.
        tcode_core::store::sweep_old_sessions(&data_dir);
        if cli.r#continue || cli.resume.is_some() {
            let resumed = tcode_core::SessionStore::resume(&data_dir, cli.resume.as_deref())
                .context("cannot resume session")?;
            let tcode_core::Resumed {
                store,
                ledger,
                checkpoints,
                startup,
                environment: previous_environment,
                delivered_environment,
            } = resumed;
            let session_id = store.id.clone();
            let ckpt_dir = data_dir.join("checkpoints").join(&session_id);
            session.checkpoints = tcode_core::CheckpointStore::load(ckpt_dir, checkpoints);
            session.ledger = ledger;
            session.ledger.attach_sink(Box::new(store));
            session.bind_scratch_session(&session_id);
            let startup =
                startup.unwrap_or_else(|| (opening_context)(&cwd, &session.tool_ctx.scratch_dir));
            session.restore_startup_context(startup, previous_environment, delivered_environment);
            session.sync_environment((environment)(&cwd), None);
        } else {
            let store = tcode_core::SessionStore::create(&data_dir, &cwd)
                .context("cannot create session log")?;
            let session_id = store.id.clone();
            session.checkpoints =
                tcode_core::CheckpointStore::new(data_dir.join("checkpoints").join(&session_id));
            session.ledger.attach_sink(Box::new(store));
            session.bind_scratch_session(&session_id);
            session.set_startup_context((opening_context)(&cwd, &session.tool_ctx.scratch_dir));
        }
    } else {
        session.set_startup_context((opening_context)(&cwd, &session.tool_ctx.scratch_dir));
    }
    // Resume restores only persistent ledger events. Re-estimate from the
    // reconstructed request before the first turn so plain REPL and --prompt
    // can auto-compact a near-full compacted session just like the TUI.
    if !session.ledger.is_empty() {
        session.last_prompt_tokens = agent.estimate_context_tokens(&session);
    }
    let line_approver = approver::LineApprover::new(cli.prompt.is_none());

    if let Some(prompt) = cli.prompt {
        run_turn(&agent, &mut session, prompt, &line_approver).await?;
        return Ok(());
    }

    // Interactive: full TUI on a real terminal, plain line REPL otherwise
    // (pipes, CI, dumb terminals).
    if interactive {
        loop {
            match tcode_tui::run(
                agent.clone(),
                session,
                menu,
                agent_menu,
                opening_context.clone(),
                environment.clone(),
                config.ui.show_reasoning,
                skills.clone(),
            )
            .await?
            {
                tcode_tui::Exit::Quit => return Ok(()),
                tcode_tui::Exit::ConfigureProvider(returned_session) => {
                    let global = Config::load_global()?;
                    let Some((updated, state)) =
                        tcode_tui::wizard::reconfigure(global, &selection.profile)?
                    else {
                        // Esc only cancels the provider wizard. The existing
                        // session and active model are still valid, so restore
                        // the TUI instead of turning a no-op into a process
                        // error.
                        session = *returned_session;
                        menu = build_menu(&config, &selection, model_cell.clone());
                        agent_menu = build_agent_menu(&config, &menu, pinned.clone());
                        continue;
                    };
                    let path = updated.write_global(CONFIG_HEADER)?;
                    state.save();

                    // Rebuild the selected provider in the existing shared
                    // model cell, then reopen the same conversation.
                    let mut runtime_config = Config::load(&cwd)?;
                    tcode_providers::hydrate_codex_models(&mut runtime_config);
                    let runtime_selection = runtime_config.select(None, None, &state)?;
                    let profile = runtime_config
                        .profiles
                        .get(&runtime_selection.profile)
                        .context("selected profile disappeared after setup")?;
                    let active = tcode_providers::build_active(
                        profile,
                        &runtime_selection,
                        &runtime_config.watchdog,
                    )?;
                    model_cell.swap(active);
                    menu = build_menu(&runtime_config, &runtime_selection, model_cell.clone());
                    agent_menu = build_agent_menu(&runtime_config, &menu, pinned.clone());
                    session = *returned_session;
                    println!("{DIM}updated {}; reopening tcode{RESET}", path.display());
                }
            }
        }
    }

    let registry = CommandRegistry::builtin();
    let snapshot = model_cell.snapshot();
    println!(
        "{DIM}tcode v{} · {} · {} · mode {} · /help lists commands{RESET}",
        env!("CARGO_PKG_VERSION"),
        snapshot.provider.name(),
        snapshot.describe(),
        session.mode.label(),
    );
    let stdin = std::io::stdin();
    'repl: loop {
        print!("\n{CYAN}› {RESET}");
        std::io::stdout().flush()?;
        let mut line = String::new();
        if stdin.read_line(&mut line)? == 0 {
            eprintln!(
                "{DIM}input closed — tcode needs an interactive terminal to keep the conversation open (for example, VS Code's Integrated Terminal rather than Debug Console).{RESET}"
            );
            break;
        }
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('/') {
            // REPL-only commands: /model drives the frontend-owned menu,
            // /help mixes it into the shared command list.
            if let Some(rest) = line.strip_prefix("/agents") {
                run_agents_command(rest.trim(), &menu, &mut agent_menu);
                continue;
            }
            if let Some(rest) = line.strip_prefix("/model") {
                run_model_command(rest.trim(), &menu, &model_cell);
                continue;
            }
            if line == "/help" {
                println!("{DIM}commands:{RESET}");
                println!(
                    "{DIM}  {:<16} switch model · adjust reasoning effort{RESET}",
                    "/model"
                );
                println!(
                    "{DIM}  {:<16} choose models for sub-agents and Auto Mode safety{RESET}",
                    "/agents"
                );
                for (name, help) in registry.entries() {
                    println!("{DIM}  {name:<16} {help}{RESET}");
                }
                for skill in &skills {
                    println!("{DIM}  /{:<15} {}{RESET}", skill.name, skill.description);
                }
                continue;
            }
            let turn_usage = session.turn_usage;
            let outcome = registry.dispatch(
                &mut CommandCtx {
                    session: &mut session,
                    opening_context: &opening_context,
                    environment: &environment,
                    turn_usage,
                },
                &line,
            );
            let Some(outcome) = outcome else {
                // Same fallback as the TUI's `dispatch_skill`: a `/name` that
                // matches neither a UI command nor the registry loads that
                // skill and starts a turn from its rendered body, instead of
                // making the model spend a tool round-trip to fetch it.
                let rest = line.trim_start_matches('/');
                let (name, args) = match rest.split_once(char::is_whitespace) {
                    Some((name, args)) => (name, args.trim()),
                    None => (rest, ""),
                };
                if let Some(skill) = skills.iter().find(|skill| skill.name == name) {
                    let body = match &skill.source {
                        tcode_tools::SkillSource::Dir(dir) => {
                            match std::fs::read_to_string(dir.join("SKILL.md")) {
                                Ok(body) => body,
                                Err(e) => {
                                    eprintln!(
                                        "{DIM}cannot read {}: {e}{RESET}",
                                        dir.join("SKILL.md").display()
                                    );
                                    continue;
                                }
                            }
                        }
                        tcode_tools::SkillSource::Builtin(body) => body.to_string(),
                    };
                    let rendered = tcode_tools::render_skill(
                        skill,
                        &body,
                        args,
                        &cwd,
                        &session.tool_ctx.scratch_dir,
                    );
                    let wrapped = tcode_tools::wrap_skill_echo(name, args, &rendered);
                    if let Err(e) = run_turn(&agent, &mut session, wrapped, &line_approver).await {
                        eprintln!("{DIM}error: {e}{RESET}");
                    }
                    continue;
                }
                println!("{DIM}unknown command {line} — /help lists commands{RESET}");
                continue;
            };
            for message in outcome.messages {
                match message.kind {
                    MessageKind::Info => println!("{DIM}{}{RESET}", message.text),
                    MessageKind::Error => eprintln!("{DIM}{}{RESET}", message.text),
                    MessageKind::Note => {
                        println!("{CYAN}Note:{RESET} {}", message.text)
                    }
                }
            }
            for effect in outcome.effects {
                match effect {
                    CommandEffect::Exit => break 'repl,
                    CommandEffect::PersistDogfood(on) => {
                        ModelState::update(|state| state.dogfood = on)
                    }
                    // The plain REPL has no input box to ghost into, so the
                    // toggle only has to be remembered, not acted on.
                    CommandEffect::PersistSuggestions(on) => {
                        ModelState::update(|state| state.suggestions = Some(on))
                    }
                    CommandEffect::Compact { focus } => {
                        let cancel = CancellationToken::new();
                        let watcher = {
                            let cancel = cancel.clone();
                            tokio::spawn(async move {
                                if tokio::signal::ctrl_c().await.is_ok() {
                                    cancel.cancel();
                                }
                            })
                        };
                        println!("{DIM}compacting…{RESET}");
                        // Same event stream a turn uses, so the summary prints
                        // through the one `Compacted` handler in the printer.
                        let (tx, rx) = tokio::sync::mpsc::channel(1);
                        let printer = tokio::spawn(printer::print_events(rx));
                        let outcome = agent
                            .compact_with_focus(&mut session, focus.as_deref(), &tx, &cancel)
                            .await;
                        drop(tx);
                        let _ = printer.await;
                        match outcome {
                            Ok(()) => {
                                let u = &session.turn_usage;
                                println!(
                                    "{DIM}history compacted · in {} | out {}{RESET}",
                                    u.input_tokens, u.output_tokens
                                );
                            }
                            Err(e) => eprintln!("{DIM}compact failed: {e}{RESET}"),
                        }
                        watcher.abort();
                    }
                    CommandEffect::ConversationCleared => {}
                    CommandEffect::ConversationReplaced => {
                        session.last_prompt_tokens = agent.estimate_context_tokens(&session);
                        println!(
                            "{DIM}session resumed · {} entries{RESET}",
                            session.ledger.len()
                        );
                    }
                    CommandEffect::OpenResumePicker => {
                        println!(
                            "{DIM}interactive resume picker needs the full TUI — use /resume <id>{RESET}"
                        );
                    }
                }
            }
            continue;
        }
        if let Err(e) = run_turn(&agent, &mut session, line, &line_approver).await {
            eprintln!("{DIM}error: {e}{RESET}");
        }
    }
    Ok(())
}

async fn run_turn(
    agent: &Agent,
    session: &mut Session,
    input: String,
    approver: &dyn tcode_core::Approver,
) -> Result<(), AgentError> {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let printer = tokio::spawn(printer::print_events(rx));

    let cancel = CancellationToken::new();
    let watcher = {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                cancel.cancel();
            }
        })
    };

    let result = agent
        .user_turn(
            session,
            vec![ContentBlock::Text { text: input }],
            &tx,
            approver,
            cancel,
        )
        .await;
    drop(tx);
    let _ = printer.await;
    watcher.abort();

    let u = &session.turn_usage;
    let cache_pct = if u.total_input() > 0 {
        (u.cache_read_tokens as f64 / u.total_input() as f64 * 100.0).round()
    } else {
        0.0
    };
    println!(
        "{DIM}· in {} | out {} | cache r {} ({cache_pct:.0}%) w {}{RESET}",
        u.input_tokens, u.output_tokens, u.cache_read_tokens, u.cache_write_tokens
    );
    result
}
