use std::io::Write as _;

use tcode_core::AgentEvent;
use tokio::sync::mpsc;

const DIM: &str = "\x1b[2m";
const CYAN: &str = "\x1b[36m";
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const RESET: &str = "\x1b[0m";

/// Color the tool name in a summary like "shell(command)" or
/// "read(path/to/file)" — the name is green, the rest is dim.
fn color_tool_summary(summary: &str) -> String {
    if let Some(paren) = summary.find('(') {
        let name = &summary[..paren];
        let args = &summary[paren..];
        format!("{GREEN}{name}{RESET}{DIM}{args}{RESET}")
    } else {
        format!("{GREEN}{summary}{RESET}")
    }
}

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
            // Tool arguments stream silently; the finished call prints via
            // ToolStart. Nothing to show in plain mode.
            AgentEvent::ToolInputDelta(_) => {}
            AgentEvent::Retrying {
                attempt,
                max,
                error,
                partial_output_retained,
                delay_ms,
            } => {
                if in_thinking {
                    print!("{RESET}");
                    in_thinking = false;
                }
                let secs = delay_ms.div_ceil(1000);
                let retained = if partial_output_retained {
                    " — incomplete response above retained; not sent back to model"
                } else {
                    ""
                };
                println!("\n{RED}[retry {attempt}/{max}] API error: {error}; retrying in {secs}s{retained}{RESET}");
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
                println!("{CYAN}●{RESET} {}", color_tool_summary(&summary));
            }
            AgentEvent::ToolBatchStart { label, calls } => {
                if in_thinking {
                    print!("{RESET}");
                    in_thinking = false;
                }
                println!("{CYAN}●{RESET} {label}");
                for (_, name, input) in calls {
                    println!(
                        "  ├ {}",
                        color_tool_summary(&tcode_core::agent::summarize_call(&name, &input))
                    );
                }
            }
            AgentEvent::ToolEnd {
                preview, is_error, ..
            } => {
                let color = if is_error { RED } else { DIM };
                println!("  {color}⎿ {preview}{RESET}");
            }
            // The original @ marker is already visible in the prompt. Plain
            // mode deliberately does not duplicate its bounded context block.
            AgentEvent::ReferencesExpanded { .. } => {}
            AgentEvent::QueuedInput { text, .. } => println!("{CYAN}›{RESET} {text}"),
            AgentEvent::Note(text) => println!("{DIM}Note: {text}{RESET}"),
            AgentEvent::UserNote {
                text,
                answer: false,
            } => println!("{CYAN}Note:{RESET} {text}"),
            AgentEvent::UserNote { answer: true, .. } => {}
            AgentEvent::Usage(_) | AgentEvent::DelegatedUsage(_) | AgentEvent::RateLimits(_) => {}
            // Cohort roster and sub-agent trace/progress are display-only
            // observability; plain mode already prints the final report through
            // the parent tool result.
            AgentEvent::CohortUpdated(_)
            | AgentEvent::TaskRunStarted { .. }
            | AgentEvent::TaskRunEvent { .. }
            | AgentEvent::TaskRunFinished { .. } => {}
            AgentEvent::Compacting => {
                println!("{YELLOW}[context near limit — compacting]{RESET}");
            }
            // Plain mode has no folds, so the summary prints in full: it is now
            // the model's only record of the conversation so far.
            AgentEvent::Compacted(summary) => {
                println!("{DIM}── earlier conversation compacted ──{RESET}");
                println!("{DIM}{summary}{RESET}");
            }
            AgentEvent::ModeChanged(mode) => {
                println!("{DIM}[permission mode → {}]{RESET}", mode.label());
            }
            AgentEvent::AutoClassifierUnavailable(reason) => {
                println!(
                    "{YELLOW}[Auto classifier unavailable; asking you instead: {reason}]{RESET}"
                );
            }
            AgentEvent::AutoModePaused(notice) => {
                println!("{YELLOW}[{notice}]{RESET}");
            }
            AgentEvent::AwaitingUserInput => {
                println!("{YELLOW}[change declined — add guidance to continue]{RESET}");
            }
            AgentEvent::StepLimitReached { max } => {
                println!("{YELLOW}[step limit reached ({max} steps) — say \"continue\" to keep going]{RESET}");
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
