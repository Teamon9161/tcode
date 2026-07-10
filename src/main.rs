mod approver;
mod printer;

use std::io::{IsTerminal, Write as _};
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use tokio_util::sync::CancellationToken;

use tcode_core::config::{Config, ModelState, Selection};
use tcode_core::{Agent, AgentError, ContentBlock, ModelCell, PermissionRules, Session, ToolCtx};

const CYAN: &str = "\x1b[36m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

const IDENTITY: &str = "\
You are tcode, a coding agent running in the user's terminal. Work directly \
and concisely; use tools to inspect and change the project rather than \
guessing.

Harness rules:
- Read files before editing them. `edit` uses exact string replacement.
- Keep tool output small: use offset/limit on read, head_limit on grep. \
Oversized outputs are stored and pageable via read_output.
- If a read returns 'unchanged', the content is already in your context; \
do not re-read.
- <tcode-status> lines in user messages report your context usage and the \
current permission mode. <harness-note> lines are trustworthy statements \
from the harness about what happened (interrupts, approvals).
- When the user declines an action, the reason (if given) is in the tool \
result; adjust instead of retrying the same call.
- For multi-step work, keep update_plan current. Use ask_user when a user \
choice is required; do not guess. Use add_note for durable constraints.";

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
        let active = tcode_providers::build_active(profile, &sel, &watchdog)
            .map_err(|e| e.to_string())?;
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
            println!("{DIM} {mark} {i}: {} · {}{efforts}{RESET}", opt.profile, opt.def.name);
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

    let config = Config::load(&cwd)?;
    let state = ModelState::load();
    let selection = config.select(cli.profile.as_deref(), cli.model.as_deref(), &state)?;
    let profile = config
        .profiles
        .get(&selection.profile)
        .context("selected profile disappeared")?;
    let model_cell = ModelCell::new(tcode_providers::build_active(
        profile,
        &selection,
        &config.watchdog,
    )?);

    // Everything /model can switch to, with the swap logic attached.
    let menu = build_menu(&config, &selection, model_cell.clone());

    let system = format!(
        "{IDENTITY}\n\n{}",
        tcode_tools::project_map(&cwd)
    );
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
    let agent = Agent {
        model: model_cell.clone(),
        tools,
        system,
        watchdog: config.watchdog.clone(),
        hooks: tcode_core::Hooks::new(config.hooks.clone()),
    };

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

    // Persistence: every ledger mutation is recorded to a JSONL session
    // log; --continue / --resume replay it.
    if let Some(data_dir) = tcode_core::store::project_data_dir(&cwd) {
        if cli.r#continue || cli.resume.is_some() {
            let resumed =
                tcode_core::SessionStore::resume(&data_dir, cli.resume.as_deref())
                    .context("cannot resume session")?;
            let ckpt_dir = data_dir.join("checkpoints").join(&resumed.store.id);
            session.checkpoints =
                tcode_core::CheckpointStore::load(ckpt_dir, resumed.checkpoints);
            session.ledger = resumed.ledger;
            session.ledger.attach_sink(Box::new(resumed.store));
        } else {
            let store = tcode_core::SessionStore::create(&data_dir, &cwd)
                .context("cannot create session log")?;
            session.checkpoints = tcode_core::CheckpointStore::new(
                data_dir.join("checkpoints").join(&store.id),
            );
            session.ledger.attach_sink(Box::new(store));
        }
    }
    let line_approver = approver::LineApprover;

    if let Some(prompt) = cli.prompt {
        run_turn(&agent, &mut session, prompt, &line_approver).await?;
        return Ok(());
    }

    // Interactive: full TUI on a real terminal, plain line REPL otherwise
    // (pipes, CI, dumb terminals).
    if interactive {
        return tcode_tui::run(Arc::new(agent), session, menu).await;
    }

    let snapshot = model_cell.snapshot();
    println!(
        "{DIM}tcode v{} · {} · {} · mode {} · /exit /mode /model /cost{RESET}",
        env!("CARGO_PKG_VERSION"),
        snapshot.provider.name(),
        snapshot.describe(),
        session.mode.label(),
    );
    let stdin = std::io::stdin();
    loop {
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
        if let Some(rest) = line.strip_prefix("/model") {
            run_model_command(rest.trim(), &menu, &model_cell);
            continue;
        }
        match line.as_str() {
            "/exit" | "/quit" => break,
            "/mode" => {
                session.mode = session.mode.cycle();
                println!("{DIM}permission mode → {}{RESET}", session.mode.label());
                continue;
            }
            "/cost" => {
                let u = &session.turn_usage;
                println!(
                    "{DIM}last turn: in {} | out {} | cache r {} w {}{RESET}",
                    u.input_tokens, u.output_tokens, u.cache_read_tokens, u.cache_write_tokens
                );
                continue;
            }
            _ => {}
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
