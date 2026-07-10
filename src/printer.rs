use std::io::Write as _;

use tcode_core::AgentEvent;
use tokio::sync::mpsc;

const DIM: &str = "\x1b[2m";
const CYAN: &str = "\x1b[36m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const RESET: &str = "\x1b[0m";

/// Line-mode event renderer. The M2 TUI is a different consumer of the
/// same event stream.
pub async fn print_events(mut rx: mpsc::Receiver<AgentEvent>) {
    let mut in_thinking = false;
    let mut wrote_text = false;
    while let Some(ev) = rx.recv().await {
        match ev {
            AgentEvent::Started => {}
            AgentEvent::TextDelta(t) => {
                if in_thinking {
                    print!("{RESET}\n\n");
                    in_thinking = false;
                }
                wrote_text = true;
                print!("{t}");
                let _ = std::io::stdout().flush();
            }
            AgentEvent::ThinkingDelta(t) => {
                if !in_thinking {
                    print!("{DIM}");
                    in_thinking = true;
                }
                print!("{t}");
                let _ = std::io::stdout().flush();
            }
            AgentEvent::Retrying {
                attempt,
                max,
                error,
            } => {
                if in_thinking {
                    print!("{RESET}");
                    in_thinking = false;
                }
                println!("\n{YELLOW}[watchdog] {error}; retrying ({attempt}/{max}) — partial output above is discarded{RESET}");
            }
            AgentEvent::ToolStart { summary, .. } => {
                if in_thinking {
                    print!("{RESET}");
                    in_thinking = false;
                }
                if wrote_text {
                    println!();
                    wrote_text = false;
                }
                println!("{CYAN}●{RESET} {summary}");
            }
            AgentEvent::ToolBatchStart { label, calls } => {
                if in_thinking {
                    print!("{RESET}");
                    in_thinking = false;
                }
                println!("{CYAN}●{RESET} {label}");
                for (name, input) in calls {
                    println!("  {DIM}├ {}{RESET}", tcode_core::agent::summarize_call(&name, &input));
                }
            }
            AgentEvent::ToolEnd {
                preview, is_error, ..
            } => {
                let color = if is_error { RED } else { DIM };
                println!("  {color}⎿ {preview}{RESET}");
            }
            AgentEvent::Usage(_) | AgentEvent::DelegatedUsage(_) | AgentEvent::RateLimits(_) => {}
            AgentEvent::Compacting => {
                println!("{YELLOW}[context near limit — compacting]{RESET}");
            }
            AgentEvent::AwaitingUserInput => {
                println!("{YELLOW}[change declined — add guidance to continue]{RESET}");
            }
            AgentEvent::Interrupted => {
                if in_thinking {
                    print!("{RESET}");
                    in_thinking = false;
                }
                println!("\n{YELLOW}[interrupted]{RESET}");
            }
            AgentEvent::TurnEnd => {
                if in_thinking {
                    print!("{RESET}");
                    in_thinking = false;
                }
                if wrote_text {
                    println!();
                }
            }
        }
    }
}
