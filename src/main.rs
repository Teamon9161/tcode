mod approver;
mod printer;

use std::io::{IsTerminal, Write as _};
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use tokio_util::sync::CancellationToken;

use tcode_core::config::Config;
use tcode_core::{Agent, AgentError, ContentBlock, PermissionRules, Session, ToolCtx};

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
result; adjust instead of retrying the same call.";

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
    /// Start in a specific permission mode (plan/default/accept-edits/auto)
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
    let config = Config::load(&cwd)?;
    let (profile_name, profile) = config.profile(cli.profile.as_deref())?;
    let provider = tcode_providers::build(&profile_name, profile, &config.watchdog, cli.model)?;

    let system = format!(
        "{IDENTITY}\n\n{}",
        tcode_tools::project_map(&cwd)
    );
    let mut tools = tcode_tools::builtin_tools();
    tools.push(Arc::new(tcode_tools::TaskTool::new(
        provider.clone(),
        config.watchdog.clone(),
        profile.max_tokens.unwrap_or(8192),
        profile.context_window.unwrap_or(200_000),
        config.limits.tool_output_tokens,
        cwd.clone(),
    )));
    let agent = Agent {
        provider,
        tools,
        system,
        max_tokens: profile.max_tokens.unwrap_or(8192),
        context_window: profile.context_window.unwrap_or(200_000),
        watchdog: config.watchdog.clone(),
        hooks: tcode_core::Hooks::new(config.hooks.clone()),
    };

    let mode = match cli.mode.as_deref() {
        Some("plan") => tcode_core::PermissionMode::Plan,
        Some("accept-edits") => tcode_core::PermissionMode::AcceptEdits,
        Some("auto") => tcode_core::PermissionMode::Auto,
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
    if std::io::stdout().is_terminal() && std::io::stdin().is_terminal() {
        return tcode_tui::run(Arc::new(agent), session).await;
    }

    println!(
        "{DIM}tcode v{} · {} · {} · mode {} · /exit /mode /cost{RESET}",
        env!("CARGO_PKG_VERSION"),
        agent.provider.name(),
        agent.provider.model(),
        session.mode.label(),
    );
    let stdin = std::io::stdin();
    loop {
        print!("\n{CYAN}› {RESET}");
        std::io::stdout().flush()?;
        let mut line = String::new();
        if stdin.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim().to_string();
        if line.is_empty() {
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
