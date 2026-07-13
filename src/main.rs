mod approver;
mod printer;

use std::io::{IsTerminal, Write as _};
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use tokio_util::sync::CancellationToken;

use tcode_core::commands::{CommandCtx, CommandEffect, CommandRegistry, MessageKind};
use tcode_core::config::{Config, ConfigError, ModelState, Selection};
use tcode_core::{Agent, AgentError, ContentBlock, ModelCell, PermissionRules, Session, ToolCtx};

const CYAN: &str = "\x1b[36m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

const INTERACTIVE_AGENT_SYSTEM: &str = include_str!("../prompts/interactive-agent-system.md");

const CONFIG_HEADER: &str = "\
# tcode global configuration — created by the setup wizard.
# Add profiles/models freely; the active choice lives in state.toml
# (written by /model). Keys: prefer api_key_env over inline api_key.

";

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
        if !profile.is_usable(pname) && pname != &selection.profile {
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
        ModelState {
            profile: Some(opt.profile.clone()),
            model: Some(opt.def.name.clone()),
            effort: effort.map(String::from),
        }
        .save();
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

#[derive(Parser)]
#[command(name = "tcode", version, about = "tcode — a terminal agent harness")]
struct Cli {
    /// Config profile to use (from ~/.tcode/config.toml)
    #[arg(long)]
    profile: Option<String>,
    /// Override the profile's model
    #[arg(long)]
    model: Option<String>,
    /// One-shot prompt: run the full agent loop, print, exit
    #[arg(short = 'p', long)]
    prompt: Option<String>,
    /// Start in a specific permission mode (plan/default/accept-edits/unsafe)
    #[arg(long)]
    mode: Option<String>,
    /// Continue the most recent session in this project
    #[arg(short = 'c', long = "continue")]
    r#continue: bool,
    /// Resume a session by id (prefix is enough)
    #[arg(long)]
    resume: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let cwd = std::env::current_dir().context("cannot determine working directory")?;
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

    let (config, selection, active_model) = loop {
        let config = Config::load(&cwd)?;
        let state = ModelState::load();
        let selection = config.select(cli.profile.as_deref(), cli.model.as_deref(), &state)?;
        let profile = config
            .profiles
            .get(&selection.profile)
            .context("selected profile disappeared")?;
        match tcode_providers::build_active(profile, &selection, &config.watchdog) {
            Ok(active) => break (config, selection, active),
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

    let system = INTERACTIVE_AGENT_SYSTEM.to_string();
    let mut tools = tcode_tools::builtin_tools();
    tools.push(Arc::new(tcode_tools::TaskTool::new(
        model_cell.clone(),
        config.watchdog.clone(),
        config.limits.tool_output_tokens,
        cwd.clone(),
    )));
    tools.push(Arc::new(tcode_tools::UpdatePlanTool));
    tools.push(Arc::new(tcode_tools::AskUserTool));
    tools.push(Arc::new(tcode_tools::AddNoteTool));
    // Registered only when skills exist, so skill-less projects pay zero
    // prompt tokens for the feature.
    if let Some(skill_tool) = tcode_tools::SkillTool::discover(&cwd) {
        tools.push(Arc::new(skill_tool));
    }
    // MCP servers from config; a broken server warns instead of blocking.
    if !config.mcp_servers.is_empty() {
        let (mcp_tools, warnings) =
            tcode_tools::connect_mcp_servers(&config.mcp_servers, &cwd).await;
        for warning in warnings {
            eprintln!("{DIM}warning: {warning}{RESET}");
        }
        tools.extend(mcp_tools);
    }
    let agent = Arc::new(Agent {
        model: model_cell.clone(),
        tools,
        system,
        watchdog: config.watchdog.clone(),
        hooks: tcode_core::Hooks::new(config.hooks.clone()),
        max_steps: config.limits.max_steps_per_turn,
    });

    let mode = match cli.mode.as_deref() {
        Some("plan") => tcode_core::PermissionMode::Plan,
        Some("accept-edits") => tcode_core::PermissionMode::AcceptEdits,
        Some("unsafe") | Some("auto") => tcode_core::PermissionMode::Unsafe,
        Some("default") => tcode_core::PermissionMode::Default,
        Some(other) => anyhow::bail!("unknown mode '{other}'"),
        None => config.permissions.mode,
    };
    let rules = PermissionRules {
        allow: config.permissions.allow.clone(),
        deny: config.permissions.deny.clone(),
    };
    let mut session = Session::new(
        ToolCtx::new(cwd.clone(), config.limits.tool_output_tokens),
        mode,
        rules,
    );
    session.set_opening_context(tcode_tools::project_map(&cwd));

    // Persistence: every ledger mutation is recorded to a JSONL session
    // log; --continue / --resume replay it.
    if let Some(data_dir) = tcode_core::store::project_data_dir(&cwd) {
        if cli.r#continue || cli.resume.is_some() {
            let resumed = tcode_core::SessionStore::resume(&data_dir, cli.resume.as_deref())
                .context("cannot resume session")?;
            let ckpt_dir = data_dir.join("checkpoints").join(&resumed.store.id);
            session.checkpoints = tcode_core::CheckpointStore::load(ckpt_dir, resumed.checkpoints);
            session.ledger = resumed.ledger;
            session.ledger.attach_sink(Box::new(resumed.store));
        } else {
            let store = tcode_core::SessionStore::create(&data_dir, &cwd)
                .context("cannot create session log")?;
            session.checkpoints =
                tcode_core::CheckpointStore::new(data_dir.join("checkpoints").join(&store.id));
            session.ledger.attach_sink(Box::new(store));
        }
    }
    let line_approver = approver::LineApprover;
    let opening_context: tcode_tui::OpeningContextFn =
        Arc::new(|path| tcode_tools::project_map(path));

    if let Some(prompt) = cli.prompt {
        run_turn(&agent, &mut session, prompt, &line_approver).await?;
        return Ok(());
    }

    // Interactive: full TUI on a real terminal, plain line REPL otherwise
    // (pipes, CI, dumb terminals).
    if interactive {
        loop {
            match tcode_tui::run(agent.clone(), session, menu, opening_context.clone()).await? {
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
                        continue;
                    };
                    let path = updated.write_global(CONFIG_HEADER)?;
                    state.save();

                    // Rebuild the selected provider in the existing shared
                    // model cell, then reopen the same conversation.
                    let runtime_config = Config::load(&cwd)?;
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
            if let Some(rest) = line.strip_prefix("/model") {
                run_model_command(rest.trim(), &menu, &model_cell);
                continue;
            }
            if line == "/help" {
                println!("{DIM}commands:{RESET}");
                println!("{DIM}  {:<16} switch model · adjust reasoning effort{RESET}", "/model");
                for (name, help) in registry.entries() {
                    println!("{DIM}  {name:<16} {help}{RESET}");
                }
                continue;
            }
            let turn_usage = session.turn_usage;
            let outcome = registry.dispatch(
                &mut CommandCtx {
                    session: &mut session,
                    opening_context: &opening_context,
                    turn_usage,
                },
                &line,
            );
            let Some(outcome) = outcome else {
                println!("{DIM}unknown command {line} — /help lists commands{RESET}");
                continue;
            };
            for message in outcome.messages {
                match message.kind {
                    MessageKind::Info => println!("{DIM}{}{RESET}", message.text),
                    MessageKind::Error => eprintln!("{DIM}{}{RESET}", message.text),
                    MessageKind::Note => {
                        println!("{DIM}› note to model — {}{RESET}", message.text)
                    }
                }
            }
            for effect in outcome.effects {
                match effect {
                    CommandEffect::Exit => break 'repl,
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
                        match agent
                            .compact_with_focus(&mut session, focus.as_deref(), &cancel)
                            .await
                        {
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
